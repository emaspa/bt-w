//! Wire format of the BT-W5 vendor HID channel (report ID 3, 64-byte payload).
//!
//! Every message in both directions is `03 5a <cmd> <len> <payload[len]>`, zero padded.
//! The firmware reuses its transmit buffer, so bytes past `len` can be stale: only
//! `payload[..len]` is meaningful.

use std::fmt;

pub const VID: u16 = 0x041e;
/// Supported dongles: USB product ID, name, and whether it has been tested on hardware.
/// The BT-W6 uses the same app and, per user reports, the same commands.
pub const MODELS: &[(u16, &str, bool)] = &[(0x3130, "BT-W5", true), (0x3132, "BT-W6", false)];
pub const USAGE_PAGE: u16 = 0xff01;
pub const REPORT_ID: u8 = 0x03;
pub const MAGIC: u8 = 0x5a;
pub const REPORT_LEN: usize = 65; // report ID + 64 bytes

/// Generic acknowledgement: payload = `<acked cmd> <status> <echo of request>`.
pub const CMD_ACK: u8 = 0x02;
/// Device information (firmware, serial, own address).
pub const CMD_INFO: u8 = 0x09;
/// Bluetooth side: settings, pairing, connection.
pub const CMD_BT: u8 = 0x6b;

// Sub-commands of CMD_BT (first payload byte).
pub const BT_PAIRED: u8 = 0x02;
pub const BT_LINK: u8 = 0x04;
pub const BT_SCAN_RESULT: u8 = 0x05;
pub const BT_CONN_EVENT: u8 = 0x06;
pub const BT_FORGET: u8 = 0x07;
pub const BT_GET: u8 = 0x09;
pub const BT_SET: u8 = 0x0a;

/// A setting is written with its `set` id and read back with `set + 5`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    /// Hands-free profile (headset mic). 1 = on, 2 = off. Changing it reboots the dongle.
    Hfp,
    /// Automatically switch to HFP when an app opens the mic. 1 = on, 0 = off.
    AutoHfp,
    /// aptX Adaptive mode. 0x20 = low latency, 0x40 = high quality.
    AptxMode,
}

impl Setting {
    pub fn set_id(self) -> u8 {
        match self {
            Setting::Hfp => 0x01,
            Setting::AutoHfp => 0x02,
            Setting::AptxMode => 0x03,
        }
    }
    pub fn get_id(self) -> u8 {
        self.set_id() + 5
    }
}

pub const APTX_LOW_LATENCY: u8 = 0x20;
pub const APTX_HIGH_QUALITY: u8 = 0x40;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub cmd: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(cmd: u8, payload: &[u8]) -> Self {
        Frame { cmd, payload: payload.to_vec() }
    }

    pub fn encode(&self) -> [u8; REPORT_LEN] {
        assert!(self.payload.len() <= REPORT_LEN - 4, "payload too long");
        let mut buf = [0u8; REPORT_LEN];
        buf[0] = REPORT_ID;
        buf[1] = MAGIC;
        buf[2] = self.cmd;
        buf[3] = self.payload.len() as u8;
        buf[4..4 + self.payload.len()].copy_from_slice(&self.payload);
        buf
    }

    /// Parse an input report (report ID included). Returns None for other reports
    /// (media keys, telephony) and malformed data.
    pub fn decode(report: &[u8]) -> Option<Frame> {
        if report.len() < 4 || report[0] != REPORT_ID || report[1] != MAGIC {
            return None;
        }
        let len = report[3] as usize;
        let end = (4 + len).min(report.len());
        Some(Frame { cmd: report[2], payload: report[4..end].to_vec() })
    }

    /// Payload of a CMD_BT frame whose sub-command is `sub`.
    pub fn bt(&self, sub: u8) -> Option<&[u8]> {
        (self.cmd == CMD_BT && self.payload.first() == Some(&sub)).then(|| &self.payload[1..])
    }

    /// `(acked cmd, status, echoed bytes)` if this is an acknowledgement.
    pub fn ack(&self) -> Option<(u8, u8, &[u8])> {
        (self.cmd == CMD_ACK && self.payload.len() >= 2)
            .then(|| (self.payload[0], self.payload[1], &self.payload[2..]))
    }
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x} [{}]", self.cmd, hex(&self.payload))
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Bluetooth device address as sent on the wire (least significant byte first).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BdAddr(pub [u8; 6]);

impl BdAddr {
    pub fn from_wire(b: &[u8]) -> Option<Self> {
        Some(BdAddr(b.get(..6)?.try_into().ok()?))
    }
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 6]
    }
}

impl fmt::Display for BdAddr {
    /// Conventional notation, most significant byte first (as BlueZ prints it).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(f, "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", b[5], b[4], b[3], b[2], b[1], b[0])
    }
}

impl fmt::Debug for BdAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::str::FromStr for BdAddr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split([':', '-']).collect();
        if parts.len() != 6 {
            return Err(format!("expected an address like 02:00:11:22:33:44, got {s:?}"));
        }
        let mut out = [0u8; 6];
        for (i, p) in parts.iter().enumerate() {
            out[5 - i] = u8::from_str_radix(p, 16).map_err(|_| format!("bad hex byte {p:?} in {s:?}"))?;
        }
        Ok(BdAddr(out))
    }
}

/// `<len> <bytes[len]>`, NUL padded. Returns the string and the bytes after the field.
fn counted_str(b: &[u8]) -> Option<(String, &[u8])> {
    let n = *b.first()? as usize;
    let field = b.get(1..1 + n).unwrap_or(&b[1..]);
    let rest = b.get(1 + n..).unwrap_or(&[]);
    Some((cstr(field), rest))
}

pub fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Connection status event (`6b .. 06 <state> <addr> <name>`), sent unsolicited and in
/// reply to `6b 01 06`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnEvent {
    pub state: u8,
    pub peer: Option<(BdAddr, String)>,
}

impl ConnEvent {
    pub fn parse(frame: &Frame) -> Option<Self> {
        let p = frame.bt(BT_CONN_EVENT)?;
        let state = *p.first()?;
        let peer = BdAddr::from_wire(&p[1..])
            .map(|addr| (addr, p.get(7..).and_then(counted_str).map(|(s, _)| s).unwrap_or_default()));
        Some(ConnEvent { state, peer })
    }

    /// Observed codes:
    /// - 0x07, no peer: idle right after a disconnect.
    /// - 0x0a: right after a disconnect or reboot, before scan results arrive.
    /// - 0x06: paging, i.e. cycling through paired headsets trying to reconnect
    ///   (LED blinks white). The peer is the headset currently being paged.
    /// - 0x08: connected. Always followed by the active profile:
    /// - 0x0b: music (A2DP, aptX). The mic sends nothing in this mode.
    /// - 0x0c: call mode (HFP, LED red). Mic audio flows, playback is voice quality.
    pub fn describe(&self) -> &'static str {
        match self.state {
            0x07 => "idle",
            0x0a => "pairing",
            0x06 => "searching",
            0x08 | 0x0b => "connected",
            0x0c => "connected, call mode",
            _ => "unknown",
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.state, 0x08 | 0x0b | 0x0c) && self.peer.is_some()
    }

    pub fn in_call(&self) -> bool {
        self.state == 0x0c
    }
}

/// Bluetooth profile and codec on the current link, for display.
///
/// The dongle has no codec query. Call mode is state 0x0c; in music mode an aptX
/// Adaptive link reports its mode (read ID 0x03). Without one the codec is aptX HD,
/// aptX or SBC, and the dongle doesn't say which.
pub fn link_description(conn: &ConnEvent, active_mode: Option<u8>) -> Option<String> {
    if !conn.is_connected() {
        return None;
    }
    if conn.in_call() {
        return Some("HFP (call audio, mic on)".into());
    }
    Some(match active_mode {
        Some(APTX_LOW_LATENCY) => "A2DP, aptX Adaptive low latency".into(),
        Some(APTX_HIGH_QUALITY) => "A2DP, aptX Adaptive high quality".into(),
        _ => "A2DP, codec not reported (aptX HD, aptX or SBC)".into(),
    })
}

/// Paired-device slot (`6b .. 02 <slot> <flags> <addr> <name>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedSlot {
    pub slot: u8,
    /// 0x80 for slots that exist, 0x00 past the end of the table.
    pub flags: u8,
    pub addr: BdAddr,
    pub name: String,
}

impl PairedSlot {
    pub fn parse(frame: &Frame) -> Option<Self> {
        let p = frame.bt(BT_PAIRED)?;
        if p.len() < 8 {
            return None;
        }
        Some(PairedSlot {
            slot: p[0],
            flags: p[1],
            addr: BdAddr::from_wire(&p[2..])?,
            name: counted_str(&p[8..]).map(|(s, _)| s).unwrap_or_default(),
        })
    }
    pub fn exists(&self) -> bool {
        self.flags & 0x80 != 0
    }
    pub fn is_empty(&self) -> bool {
        self.addr.is_zero()
    }
}

/// Device found while the dongle is in pairing mode (`6b .. 05 <addr> <name> <rssi i16>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanResult {
    pub addr: BdAddr,
    pub name: String,
    pub rssi: Option<i16>,
}

impl ScanResult {
    pub fn parse(frame: &Frame) -> Option<Self> {
        let p = frame.bt(BT_SCAN_RESULT)?;
        let addr = BdAddr::from_wire(p)?;
        let (name, rest) = counted_str(p.get(6..)?)?;
        let rssi = rest.get(..2).map(|b| i16::from_le_bytes([b[0], b[1]]));
        Some(ScanResult { addr, name, rssi })
    }
}

/// One-line human description of any frame, for `monitor`.
pub fn describe(frame: &Frame) -> String {
    if let Some((cmd, status, echo)) = frame.ack() {
        let s = if status == 0 { "ok".to_string() } else { format!("error 0x{status:02x}") };
        return format!("ack cmd 0x{cmd:02x} [{}]: {s}", hex(echo));
    }
    if let Some(e) = ConnEvent::parse(frame) {
        return match &e.peer {
            Some((a, n)) => format!("connection: {} (state 0x{:02x}) {a} {n:?}", e.describe(), e.state),
            None => format!("connection: {} (state 0x{:02x})", e.describe(), e.state),
        };
    }
    if let Some(s) = ScanResult::parse(frame) {
        let rssi = s.rssi.map(|r| format!(" rssi {r} dBm")).unwrap_or_default();
        return format!("found: {} {:?}{rssi}", s.addr, s.name);
    }
    if let Some(s) = PairedSlot::parse(frame) {
        return format!("paired slot {}: {} {:?}", s.slot, s.addr, s.name);
    }
    if let Some(p) = frame.bt(BT_GET) {
        return format!("setting 0x{:02x} = [{}]", p.first().copied().unwrap_or(0), hex(p.get(1..).unwrap_or(&[])));
    }
    format!("frame {frame}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(hexstr: &str) -> Vec<u8> {
        let mut v: Vec<u8> = (0..hexstr.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hexstr[i..i + 2], 16).unwrap())
            .collect();
        v.resize(REPORT_LEN, 0);
        v
    }

    #[test]
    fn encode_matches_original_script() {
        let f = Frame::new(CMD_BT, &[BT_SET, Setting::AptxMode.set_id(), APTX_HIGH_QUALITY]);
        assert_eq!(&f.encode()[..7], &[0x03, 0x5a, 0x6b, 0x03, 0x0a, 0x03, 0x40]);
        assert!(f.encode()[7..].iter().all(|&b| b == 0));
    }

    #[test]
    fn decode_ignores_stale_bytes() {
        // From the USB capture: len 3, followed by leftovers of an earlier message.
        let f = Frame::decode(&report("035a6b03090701020100")).unwrap();
        assert_eq!(f.payload, vec![0x09, 0x07, 0x01]);
    }

    #[test]
    fn ack() {
        let f = Frame::decode(&report("035a020a6b000a0340")).unwrap();
        assert_eq!(f.ack(), Some((0x6b, 0x00, &[0x0a, 0x03, 0x40, 0, 0, 0, 0, 0][..])));
        let f = Frame::decode(&report("035a020a6b8009")).unwrap();
        assert_eq!(f.ack().unwrap().1, 0x80);
    }

    #[test]
    fn conn_event() {
        let f = Frame::decode(&report(
            "035a6b2906064433221100022041757276616e612041636500000000000000000000000000000000000000",
        ))
        .unwrap();
        let e = ConnEvent::parse(&f).unwrap();
        assert_eq!(e.state, 0x06);
        let (addr, name) = e.peer.unwrap();
        assert_eq!(addr.to_string(), "02:00:11:22:33:44");
        assert_eq!(name, "Aurvana Ace");

        // Call mode (HFP), seen after a double-press of the dongle's button.
        let f = Frame::decode(&report("035a6b29060c88776655000220")).unwrap();
        let e = ConnEvent::parse(&f).unwrap();
        assert!(e.is_connected() && e.in_call());

        let f = Frame::decode(&report("035a6b020607")).unwrap();
        let e = ConnEvent::parse(&f).unwrap();
        assert_eq!((e.state, &e.peer), (0x07, &None));
        assert_eq!(e.describe(), "idle");
    }

    #[test]
    fn link() {
        let peer = Some(("02:00:11:22:33:44".parse().unwrap(), "Ace".to_string()));
        let music = ConnEvent { state: 0x0b, peer: peer.clone() };
        let call = ConnEvent { state: 0x0c, peer };
        assert_eq!(link_description(&music, Some(0x20)).unwrap(), "A2DP, aptX Adaptive low latency");
        assert!(link_description(&music, None).unwrap().contains("not reported"));
        assert!(link_description(&call, Some(0x20)).unwrap().starts_with("HFP"));
        assert_eq!(link_description(&ConnEvent { state: 0x06, peer: None }, None), None);
    }

    #[test]
    fn paired_slot() {
        let f = Frame::decode(&report("035a6b2d0201808877665500021f41757276616e61204163652032")).unwrap();
        let s = PairedSlot::parse(&f).unwrap();
        assert_eq!((s.slot, s.exists()), (1, true));
        assert_eq!(s.addr.to_string(), "02:00:55:66:77:88");
        assert_eq!(s.name, "Aurvana Ace 2");

        let f = Frame::decode(&report("035a6b2d020280000000000000001f")).unwrap();
        assert!(PairedSlot::parse(&f).unwrap().is_empty());
    }

    #[test]
    fn scan_result() {
        // From the capture: Tranya X3, RSSI -36.
        let f = Frame::decode(&report("035a6b0d0568001abc5ceb025833dcff")).unwrap();
        let s = ScanResult::parse(&f).unwrap();
        assert_eq!(s.name, "X3");
        assert_eq!(s.rssi, Some(-36));
        assert_eq!(s.addr.0, [0x68, 0x00, 0x1a, 0xbc, 0x5c, 0xeb]);
    }

    #[test]
    fn addr_roundtrip() {
        let a: BdAddr = "02:00:11:22:33:44".parse().unwrap();
        assert_eq!(a.0, [0x44, 0x33, 0x22, 0x11, 0x00, 0x02]);
        assert_eq!(a.to_string(), "02:00:11:22:33:44");
        assert!("nope".parse::<BdAddr>().is_err());
    }

    #[test]
    fn setting_ids() {
        assert_eq!(Setting::Hfp.get_id(), 0x06);
        assert_eq!(Setting::AutoHfp.get_id(), 0x07);
        assert_eq!(Setting::AptxMode.get_id(), 0x08);
    }
}
