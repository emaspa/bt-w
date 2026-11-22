mod device;
mod proto;
#[cfg(target_os = "linux")]
mod tray;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use device::Dongle;
use proto::*;

/// Configure a Creative BT-W5 or BT-W6 Bluetooth audio dongle.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Print every frame exchanged with the dongle.
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show firmware, settings, connected headset and pairings (default).
    Status,
    /// Set aptX Adaptive mode.
    Mode { mode: AptxMode },
    /// Enable or disable the hands-free profile (headset mic). The dongle restarts.
    Hfp { state: OnOff },
    /// Switch to HFP automatically when an app opens the microphone.
    AutoHfp { state: OnOff },
    /// BT-W6: prefer LE Audio over Bluetooth Classic for headsets that do both. The headset reconnects.
    LeAudio { state: OnOff },
    /// BT-W6: Gaming Audio Profile, low latency over LE Audio.
    Gmap { state: OnOff },
    /// BT-W6: LE Audio broadcast, for any number of listening headsets.
    Broadcast {
        #[command(subcommand)]
        action: BroadcastCmd,
    },
    /// List paired headsets.
    Paired,
    /// Disconnect the headset, enter pairing mode and list devices in range.
    Scan {
        /// Seconds to listen for devices.
        #[arg(short, long, default_value_t = 30)]
        secs: u64,
        /// Connect as soon as a device with this address or name shows up.
        #[arg(short, long)]
        connect: Option<String>,
    },
    /// Connect to a headset by address (pair it first with `scan`).
    Connect { addr: BdAddr },
    /// Disconnect the current headset; the dongle enters pairing mode.
    Disconnect,
    /// Erase every pairing stored on the dongle.
    ForgetAll {
        /// Required: confirms that all pairings should be erased.
        #[arg(long)]
        yes: bool,
    },
    /// Print everything the dongle sends until Ctrl-C.
    Monitor,
    /// Run as a system tray icon.
    #[cfg(target_os = "linux")]
    Tray,
    /// Send a raw frame (`<cmd> <payload...>` in hex, e.g. `6b 09 08`) and print replies.
    Raw {
        #[arg(required = true)]
        bytes: Vec<String>,
    },
}

#[derive(Subcommand)]
enum BroadcastCmd {
    /// Start broadcasting. The connected headset is dropped.
    On,
    /// Stop broadcasting; the dongle reconnects to its paired headsets.
    Off,
    /// Set the broadcast name listeners see.
    Name { name: String },
    /// Require the broadcast code to listen.
    Encrypt { state: OnOff },
    /// Set the broadcast code.
    Code { code: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum AptxMode {
    /// Low latency (games, video).
    Ll,
    /// High quality (music).
    Hq,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

fn main() -> Result<()> {
    // Die quietly on a closed pipe (`bt-w status | head`) like other CLI tools.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    #[cfg(target_os = "linux")]
    if matches!(cli.cmd, Some(Cmd::Tray)) {
        return tray::run();
    }
    let mut d = Dongle::open()?;
    d.verbose = cli.verbose;

    match cli.cmd.unwrap_or(Cmd::Status) {
        Cmd::Status => status(&d),
        Cmd::Mode { mode } => {
            let v = match mode {
                AptxMode::Ll => APTX_LOW_LATENCY,
                AptxMode::Hq => APTX_HIGH_QUALITY,
            };
            // While no headset is connected the dongle accepts the write, then falls
            // back to high quality, so only switch over a live link.
            let c = d.connection()?;
            if !c.is_connected() {
                bail!("no headset connected; connect one first (the dongle drops the mode otherwise)");
            }
            if c.is_le_audio() {
                bail!("the headset is on LE Audio; aptX needs Bluetooth Classic (`bt-w le-audio off`)");
            }
            d.set(Setting::AptxMode, v)?;
            // The link renegotiates; wait for the active mode to follow.
            let deadline = Instant::now() + Duration::from_secs(5);
            let active = loop {
                let a = d.active_mode();
                if a == Some(v) || Instant::now() >= deadline {
                    break a;
                }
                std::thread::sleep(Duration::from_millis(250));
            };
            match active {
                Some(a) if a == v => println!("aptX Adaptive: {}", aptx_name(a)),
                Some(a) => bail!("requested {}, but the link is running {}", aptx_name(v), aptx_name(a)),
                None => bail!("headset is not using aptX Adaptive (no active mode reported)"),
            }
            Ok(())
        }
        Cmd::Hfp { state } => {
            let v = match state {
                OnOff::On => 1,
                OnOff::Off => 2,
            };
            if d.get(Setting::Hfp)? == v {
                println!("HFP already {}", on_off(v == 1));
                return Ok(());
            }
            d.set(Setting::Hfp, v)?;
            println!("HFP {}; the dongle is restarting and will reconnect the headset.", on_off(v == 1));
            Ok(())
        }
        Cmd::AutoHfp { state } => {
            d.set(Setting::AutoHfp, matches!(state, OnOff::On) as u8)?;
            println!("Auto HFP: {}", on_off(d.get(Setting::AutoHfp)? == 1));
            Ok(())
        }
        Cmd::LeAudio { state } => {
            let v = if matches!(state, OnOff::On) { BT_MODE_LE_AUDIO } else { BT_MODE_CLASSIC };
            if d.get(Setting::BtMode)? == v {
                println!("Bluetooth mode is already {}", bt_mode_name(v));
                return Ok(());
            }
            d.set(Setting::BtMode, v)?;
            println!("Bluetooth mode: {}. Reconnecting the headset...", bt_mode_name(v));
            // `08` (connected) comes first; wait for the profile state that follows it.
            // Without a reconnect within the timeout, the link may simply not have changed.
            let e = match wait_for(&d, Duration::from_secs(15), |e| e.is_connected() && e.state != 0x08)? {
                Some(e) => e,
                None => d.connection()?,
            };
            match link_description(&e, d.active_mode()) {
                Some(link) => println!("Protocol:    {link}"),
                None => eprintln!("The headset has not reconnected yet."),
            }
            Ok(())
        }
        Cmd::Gmap { state } => {
            d.set_gmap(matches!(state, OnOff::On))?;
            println!("GMAP: {}", on_off(d.get(Setting::Gmap)? == 1));
            Ok(())
        }
        Cmd::Broadcast { action } => broadcast(&d, action),
        Cmd::Paired => {
            print_paired(&d.paired()?);
            Ok(())
        }
        Cmd::Scan { secs, connect } => scan(&d, Duration::from_secs(secs), connect.as_deref()),
        Cmd::Connect { addr } => connect(&d, addr),
        Cmd::Disconnect => {
            d.disconnect()?;
            println!("Disconnected; the dongle is in pairing mode.");
            Ok(())
        }
        Cmd::ForgetAll { yes } => {
            if !yes {
                bail!("this erases every pairing on the dongle; pass --yes to confirm");
            }
            d.forget_all()?;
            println!("All pairings erased.");
            Ok(())
        }
        Cmd::Monitor => loop {
            if let Some(f) = d.recv(Duration::from_secs(3600))? {
                println!("{}", describe(&f));
            }
        },
        Cmd::Raw { bytes } => raw(&d, &bytes),
        #[cfg(target_os = "linux")]
        Cmd::Tray => unreachable!("handled above"),
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn aptx_name(v: u8) -> String {
    match v {
        APTX_LOW_LATENCY => "low latency".into(),
        APTX_HIGH_QUALITY => "high quality".into(),
        v => format!("unknown (0x{v:02x})"),
    }
}

fn bt_mode_name(v: u8) -> String {
    match v {
        BT_MODE_CLASSIC => "Classic".into(),
        BT_MODE_LE_AUDIO => "LE Audio".into(),
        v => format!("unknown (0x{v:02x})"),
    }
}

/// Read events until one matches `done`, or `timeout` passes.
fn wait_for(d: &Dongle, timeout: Duration, done: impl Fn(&ConnEvent) -> bool) -> Result<Option<ConnEvent>> {
    let deadline = Instant::now() + timeout;
    while let Some(f) = d.recv(deadline.saturating_duration_since(Instant::now()))? {
        if let Some(e) = ConnEvent::parse(&f)
            && done(&e)
        {
            return Ok(Some(e));
        }
    }
    Ok(None)
}

fn broadcast(d: &Dongle, action: BroadcastCmd) -> Result<()> {
    match action {
        BroadcastCmd::On | BroadcastCmd::Off => {
            let on = matches!(action, BroadcastCmd::On);
            if d.connection()?.is_broadcasting() == on {
                println!("Broadcast already {}", on_off(on));
                return Ok(());
            }
            d.toggle_broadcast()?;
            if wait_for(d, Duration::from_secs(10), |e| e.is_broadcasting() == on)?.is_none() {
                bail!("the dongle did not {} the broadcast", if on { "start" } else { "stop" });
            }
            if on {
                println!("Broadcasting as {:?}.", d.get_text(TextSetting::BroadcastName)?);
            } else {
                println!("Broadcast off; the dongle is reconnecting to its paired headsets.");
            }
        }
        BroadcastCmd::Name { name } => {
            if name.is_empty() || name.len() > 32 || !name.is_ascii() {
                bail!("the name must be 1 to 32 ASCII characters");
            }
            d.set_text(TextSetting::BroadcastName, &name)?;
            println!("Broadcast name: {name:?}");
        }
        BroadcastCmd::Encrypt { state } => {
            d.set(Setting::BroadcastEncrypt, matches!(state, OnOff::On) as u8)?;
            println!("Broadcast encryption: {}", on_off(d.get(Setting::BroadcastEncrypt)? == 1));
        }
        BroadcastCmd::Code { code } => {
            // LE Audio broadcast codes are at most 16 bytes.
            if code.len() < 4 || code.len() > 16 || !code.is_ascii() {
                bail!("the code must be 4 to 16 ASCII characters");
            }
            d.set_text(TextSetting::BroadcastCode, &code)?;
            println!("Broadcast code set.");
        }
    }
    Ok(())
}

fn status(d: &Dongle) -> Result<()> {
    let note = if d.tested { "" } else { " (untested model)" };
    println!("Model:       Creative {}{note}", d.model);
    println!("Firmware:    {}", d.firmware()?);
    println!("Serial:      {}", d.serial()?);
    println!("Address:     {}", d.own_address()?);
    let c = d.connection()?;
    let active = d.active_mode();
    match &c.peer {
        Some((addr, name)) if c.is_connected() => {
            println!("Headset:     {name} ({addr})");
            if let Some(link) = link_description(&c, active) {
                println!("Protocol:    {link}");
            }
        }
        Some((addr, name)) => println!("Headset:     not connected, {} ({name}, {addr})", c.describe()),
        None => println!("Headset:     none ({})", c.describe()),
    }
    println!("aptX mode:   {}", aptx_name(d.get(Setting::AptxMode)?));
    let hfp = d.get(Setting::Hfp)?;
    println!("HFP:         {}", if hfp == 1 { "allowed" } else { "off" });
    println!("Auto HFP:    {}", on_off(d.get(Setting::AutoHfp)? == 1));
    // BT-W6 only: the BT-W5 rejects these reads.
    if let Ok(mode) = d.get(Setting::BtMode) {
        println!("Bluetooth:   {} preferred", bt_mode_name(mode));
        println!("GMAP:        {}", on_off(d.get(Setting::Gmap)? == 1));
        let name = d.get_text(TextSetting::BroadcastName)?;
        let encrypted = d.get(Setting::BroadcastEncrypt)? == 1;
        println!(
            "Broadcast:   {}, name {name:?}, {}",
            on_off(c.is_broadcasting()),
            if encrypted { "encrypted" } else { "unencrypted" }
        );
    }
    print_paired(&d.paired()?);
    Ok(())
}

fn print_paired(slots: &[PairedSlot]) {
    let used: Vec<_> = slots.iter().filter(|s| !s.is_empty()).collect();
    println!("Paired:      {} of {} slots", used.len(), slots.len());
    for s in used {
        println!("  [{}] {} {}", s.slot, s.addr, s.name);
    }
}

fn matches_target(r: &ScanResult, target: &str) -> bool {
    match target.parse::<BdAddr>() {
        Ok(a) => a == r.addr,
        Err(_) => r.name.eq_ignore_ascii_case(target),
    }
}

fn scan(d: &Dongle, secs: Duration, target: Option<&str>) -> Result<()> {
    d.disconnect()?;
    eprintln!("Pairing mode: put the headset in pairing mode. Listening for {}s...", secs.as_secs());
    let deadline = Instant::now() + secs;
    let mut seen: HashMap<BdAddr, ScanResult> = HashMap::new();
    while let Some(f) = d.recv(deadline.saturating_duration_since(Instant::now()))? {
        let Some(r) = ScanResult::parse(&f) else { continue };
        if seen.insert(r.addr, r.clone()).is_none() {
            let rssi = r.rssi.map(|v| format!("  {v} dBm")).unwrap_or_default();
            println!("{}  {}{rssi}", r.addr, r.name);
        }
        if let Some(t) = target
            && matches_target(&r, t)
        {
            return connect(d, r.addr);
        }
    }
    if let Some(t) = target {
        bail!("{t:?} did not show up within {}s", secs.as_secs());
    }
    if seen.is_empty() {
        eprintln!("No devices found.");
    } else {
        eprintln!("Connect with: bt-w connect <ADDRESS>");
    }
    Ok(())
}

fn connect(d: &Dongle, addr: BdAddr) -> Result<()> {
    d.connect(addr)?;
    eprintln!("Connecting to {addr}...");
    let deadline = Instant::now() + Duration::from_secs(20);
    while let Some(f) = d.recv(deadline.saturating_duration_since(Instant::now()))? {
        if let Some(e) = ConnEvent::parse(&f)
            && let Some((a, name)) = &e.peer
            && *a == addr
            && e.is_connected()
        {
            println!("Connected to {name} ({addr}).");
            return Ok(());
        }
    }
    bail!("no connection to {addr} within 20s (is the headset in pairing mode?)")
}

fn raw(d: &Dongle, words: &[String]) -> Result<()> {
    let bytes = words
        .iter()
        .flat_map(|w| w.split([' ', ',']))
        .filter(|w| !w.is_empty())
        .map(|w| u8::from_str_radix(w.trim_start_matches("0x"), 16).with_context(|| format!("bad hex byte {w:?}")))
        .collect::<Result<Vec<u8>>>()?;
    let (cmd, payload) = bytes.split_first().context("need at least a command byte")?;
    d.send(&Frame::new(*cmd, payload))?;
    while let Some(f) = d.recv(Duration::from_millis(1000))? {
        println!("{f}    {}", describe(&f));
    }
    Ok(())
}
