//! HID transport. Uses the OS HID driver (hidraw on Linux), so the dongle's media
//! keys keep working while we talk to it.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hidapi::{HidApi, HidDevice};

use crate::proto::*;

pub struct Dongle {
    dev: HidDevice,
    /// Print every frame sent and received.
    pub verbose: bool,
}

const REPLY_TIMEOUT: Duration = Duration::from_millis(800);

impl Dongle {
    pub fn open() -> Result<Self> {
        let api = HidApi::new().context("initialising HID")?;
        let candidates: Vec<_> =
            api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == PID).collect();
        if candidates.is_empty() {
            bail!("no Creative BT-W5 found (USB {VID:04x}:{PID:04x})");
        }
        // The config channel is the vendor collection on interface 0. Linux reports one
        // entry per collection with the same path; other OSes give it its own path.
        let info = candidates
            .iter()
            .find(|d| d.usage_page() == USAGE_PAGE)
            .or_else(|| candidates.iter().find(|d| d.interface_number() == 0))
            .ok_or_else(|| anyhow!("BT-W5 found, but not its configuration interface"))?;
        let dev = info.open_device(&api).map_err(|e| {
            anyhow!(
                "cannot open {}: {e}\n\
                 On Linux, install the udev rule (see README) or run as root.",
                info.path().to_string_lossy()
            )
        })?;
        Ok(Dongle { dev, verbose: false })
    }

    pub fn send(&self, frame: &Frame) -> Result<()> {
        if self.verbose {
            eprintln!("-> {frame}");
        }
        self.dev.write(&frame.encode()).context("writing to the dongle")?;
        Ok(())
    }

    /// Next frame from the dongle, or None after `timeout`.
    pub fn recv(&self, timeout: Duration) -> Result<Option<Frame>> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; REPORT_LEN];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ms = left.as_millis().min(i32::MAX as u128) as i32;
            let n = self.dev.read_timeout(&mut buf, ms).context("reading from the dongle")?;
            if n == 0 {
                return Ok(None);
            }
            if let Some(f) = Frame::decode(&buf[..n]) {
                if self.verbose {
                    eprintln!("<- {f}");
                }
                return Ok(Some(f));
            }
            // Media key / telephony report: not ours.
            if left.is_zero() {
                return Ok(None);
            }
        }
    }

    /// Send `req` and return the first frame for which `pick` yields a value.
    /// An error ack for the same command ends the wait early.
    pub fn request<T>(&self, req: &Frame, mut pick: impl FnMut(&Frame) -> Option<T>) -> Result<T> {
        self.send(req)?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        while let Some(f) = self.recv(deadline.saturating_duration_since(Instant::now()))? {
            if let Some(v) = pick(&f) {
                return Ok(v);
            }
            if let Some((cmd, status, _)) = f.ack()
                && cmd == req.cmd
                && status != 0
            {
                bail!("dongle rejected [{}]: status 0x{status:02x}", hex(&req.payload));
            }
        }
        bail!("no reply to [{}] from the dongle", hex(&req.payload))
    }

    /// Send a command that only gets an ack.
    pub fn command(&self, cmd: u8, payload: &[u8]) -> Result<()> {
        let req = Frame::new(cmd, payload);
        let status = self.request(&req, |f| f.ack().filter(|a| a.0 == cmd).map(|a| a.1))?;
        if status != 0 {
            bail!("dongle rejected [{}]: status 0x{status:02x}", hex(payload));
        }
        Ok(())
    }

    // --- device info -------------------------------------------------------------

    fn info(&self, payload: &[u8], key: u8) -> Result<Vec<u8>> {
        self.request(&Frame::new(CMD_INFO, payload), |f| {
            (f.cmd == CMD_INFO && f.payload.first() == Some(&key)).then(|| f.payload[1..].to_vec())
        })
    }

    pub fn firmware(&self) -> Result<String> {
        let p = self.info(&[0x02], 0x02)?;
        let n = *p.first().unwrap_or(&0) as usize;
        Ok(cstr(p.get(1..1 + n).unwrap_or(p.get(1..).unwrap_or(&[]))))
    }

    pub fn serial(&self) -> Result<String> {
        Ok(cstr(&self.info(&[0x03], 0x03)?))
    }

    pub fn own_address(&self) -> Result<BdAddr> {
        let p = self.info(&[0x01, 0x00], 0x01)?;
        // Reply: 01 00 <addr>
        BdAddr::from_wire(p.get(1..).unwrap_or(&[])).ok_or_else(|| anyhow!("short address reply"))
    }

    // --- Bluetooth side ----------------------------------------------------------

    /// Raw value of a BT parameter (`6b 09 <id>`).
    pub fn get_param(&self, id: u8) -> Result<Vec<u8>> {
        self.request(&Frame::new(CMD_BT, &[BT_GET, id]), |f| {
            f.bt(BT_GET).filter(|p| p.first() == Some(&id)).map(|p| p[1..].to_vec())
        })
    }

    pub fn get(&self, s: Setting) -> Result<u8> {
        self.get_param(s.get_id())?
            .first()
            .copied()
            .ok_or_else(|| anyhow!("empty reply for {s:?}"))
    }

    /// aptX Adaptive mode running on the current link (param 0x03). None when no
    /// headset is connected: the dongle rejects the read then.
    pub fn active_mode(&self) -> Option<u8> {
        self.get_param(0x03).ok()?.first().copied()
    }

    pub fn set(&self, s: Setting, value: u8) -> Result<()> {
        self.command(CMD_BT, &[BT_SET, s.set_id(), value])
    }

    /// Current connection state. A connected dongle answers with `08` and then the
    /// active profile (`0b` music, `0c` call), so keep reading briefly for the latter.
    pub fn connection(&self) -> Result<ConnEvent> {
        let mut ev = self.request(&Frame::new(CMD_BT, &[BT_CONN_EVENT]), ConnEvent::parse)?;
        if ev.state == 0x08 {
            let deadline = Instant::now() + Duration::from_millis(150);
            while let Some(f) = self.recv(deadline.saturating_duration_since(Instant::now()))? {
                if let Some(next) = ConnEvent::parse(&f) {
                    ev = next;
                    if ev.state != 0x08 {
                        break;
                    }
                }
            }
        }
        Ok(ev)
    }

    pub fn paired(&self) -> Result<Vec<PairedSlot>> {
        let slots = self.get_param(0x01).ok().and_then(|v| v.first().copied()).unwrap_or(4);
        let mut out = Vec::new();
        for i in 0..slots {
            let s = self.request(&Frame::new(CMD_BT, &[BT_PAIRED, i]), |f| {
                PairedSlot::parse(f).filter(|s| s.slot == i)
            })?;
            if !s.exists() {
                break;
            }
            out.push(s);
        }
        Ok(out)
    }

    /// Drop the current headset and enter pairing mode.
    pub fn disconnect(&self) -> Result<()> {
        self.command(CMD_BT, &[BT_LINK, 0x01])
    }

    pub fn connect(&self, addr: BdAddr) -> Result<()> {
        let mut p = vec![BT_LINK, 0x02];
        p.extend_from_slice(&addr.0);
        self.command(CMD_BT, &p)
    }

    pub fn forget_all(&self) -> Result<()> {
        self.command(CMD_BT, &[BT_FORGET, 0xff])
    }
}
