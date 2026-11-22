//! HID transport. Uses the OS HID driver (hidraw on Linux), so the dongle's media
//! keys keep working while we talk to it.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hidapi::{HidApi, HidDevice};

use crate::proto::*;

pub struct Dongle {
    dev: HidDevice,
    /// "BT-W5" or "BT-W6".
    pub model: &'static str,
    /// Whether this model has been checked against real hardware.
    pub tested: bool,
    /// Print every frame sent and received.
    pub verbose: bool,
}

const REPLY_TIMEOUT: Duration = Duration::from_millis(800);

impl Dongle {
    pub fn open() -> Result<Self> {
        let api = HidApi::new().context("initialising HID")?;
        let model_of = |pid| MODELS.iter().find(|m| m.0 == pid);
        let candidates: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && model_of(d.product_id()).is_some())
            .collect();
        if candidates.is_empty() {
            bail!("no Creative BT-W5 or BT-W6 found");
        }
        // The config channel is the vendor collection on interface 0. Linux reports one
        // entry per collection with the same path; other OSes give it its own path.
        let info = candidates
            .iter()
            .find(|d| d.usage_page() == USAGE_PAGE)
            .or_else(|| candidates.iter().find(|d| d.interface_number() == 0))
            .ok_or_else(|| anyhow!("dongle found, but not its configuration interface"))?;
        let &(_, model, tested) = model_of(info.product_id()).expect("filtered above");
        let dev = info.open_device(&api).map_err(|e| {
            anyhow!(
                "cannot open {}: {e}\n\
                 On Linux, install the udev rule (see README) or run as root.",
                info.path().to_string_lossy()
            )
        })?;
        Ok(Dongle { dev, model, tested, verbose: false })
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
        let mut msg = format!("no reply to [{}] from the {}", hex(&req.payload), self.model);
        if !self.tested {
            // Settings writes may still work: only the replies are missing.
            msg.push_str(&format!(
                "\nThe {} is untested; its replies may not reach this driver. \
                 Please report the output of `bt-w -v monitor` while running another command.",
                self.model
            ));
        }
        bail!(msg)
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

    /// GMAP (BT-W6). Writing 1 while GMAP is already on turns it off, and the reply is
    /// sometimes the old value or missing, so skip no-op writes and poll for the result.
    pub fn set_gmap(&self, on: bool) -> Result<()> {
        let is_on = |d: &Self| -> Result<bool> { Ok(d.get(Setting::Gmap)? == 1) };
        if is_on(self)? == on {
            return Ok(());
        }
        self.send(&Frame::new(CMD_BT, &[BT_SET, Setting::Gmap.set_id(), on as u8]))?;
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
            if is_on(self)? == on {
                return Ok(());
            }
        }
        bail!("the dongle did not turn GMAP {}", if on { "on" } else { "off" })
    }

    /// Value of a string setting (BT-W6 broadcast name and code).
    pub fn get_text(&self, s: TextSetting) -> Result<String> {
        let v = self.get_param(s.get_id())?;
        Ok(counted_str(&v).map(|(t, _)| t).unwrap_or_default())
    }

    /// Write a string setting. The dongle doesn't ack these, so read it back.
    pub fn set_text(&self, s: TextSetting, text: &str) -> Result<()> {
        let mut p = vec![BT_SET, s.set_id(), text.len() as u8];
        p.extend_from_slice(text.as_bytes());
        self.send(&Frame::new(CMD_BT, &p))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            if self.get_text(s)? == text {
                return Ok(());
            }
        }
        bail!("the dongle did not take the new {s:?}")
    }

    /// Start or stop the LE Audio broadcast (BT-W6). Both use the same toggle frame, so
    /// check the connection state first.
    pub fn toggle_broadcast(&self) -> Result<()> {
        self.send(&Frame::new(CMD_BT, &BROADCAST_TOGGLE))
    }

    /// aptX Adaptive mode running on the current link. The BT-W5 reports it in
    /// param 0x03, the BT-W6 in 0x0c (it rejects 0x03). None when no headset is
    /// connected: both dongles reject the read then.
    pub fn active_mode(&self) -> Option<u8> {
        [0x03, 0x0c]
            .into_iter()
            .find_map(|id| self.get_param(id).ok()?.first().copied())
    }

    /// Write a setting. Most writes get an ack; some BT-W6 ones (GMAP) only get the new
    /// value pushed back as a `6b 09 <get id> <value>` frame, which counts too.
    pub fn set(&self, s: Setting, value: u8) -> Result<()> {
        let req = Frame::new(CMD_BT, &[BT_SET, s.set_id(), value]);
        let status = self.request(&req, |f| {
            f.ack()
                .filter(|a| a.0 == CMD_BT)
                .map(|a| a.1)
                .or_else(|| f.bt(BT_GET).filter(|p| p.first() == Some(&s.get_id())).map(|_| 0))
        })?;
        if status != 0 {
            bail!("dongle rejected [{}]: status 0x{status:02x}", hex(&req.payload));
        }
        Ok(())
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
            // The BT-W5 reports an unused slot as empty; the BT-W6 rejects the read.
            let s = match self.request(&Frame::new(CMD_BT, &[BT_PAIRED, i]), |f| {
                PairedSlot::parse(f).filter(|s| s.slot == i)
            }) {
                Ok(s) => s,
                Err(e) if e.to_string().contains("rejected") => PairedSlot { slot: i, flags: 0x80, addr: BdAddr([0; 6]), name: String::new() },
                Err(e) => return Err(e),
            };
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
