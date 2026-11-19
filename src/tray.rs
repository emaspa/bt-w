//! System tray icon (StatusNotifierItem, as used by KDE Plasma and most Linux panels).
//!
//! A worker thread owns the dongle: it applies menu actions, listens for the events the
//! dongle sends on its own (headset connects, mode changes, scan results) and pushes a
//! fresh `State` to the tray. The icon is drawn in the colour the dongle's LED shows.

use std::fs::File;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem};
use ksni::{Icon, MenuItem, ToolTip};

use crate::device::Dongle;
use crate::proto::*;

#[derive(Clone, Default)]
struct State {
    present: bool,
    /// Model of the open dongle ("BT-W5", "BT-W6").
    model: Option<&'static str>,
    error: Option<String>,
    conn: Option<ConnEvent>,
    aptx_set: Option<u8>,
    aptx_active: Option<u8>,
    hfp: Option<bool>,
    auto_hfp: Option<bool>,
    paired: Vec<PairedSlot>,
    /// Dongle is in pairing mode; `found` lists headsets seen since.
    pairing: bool,
    found: Vec<ScanResult>,
    /// Long-running action in progress, shown in the menu header.
    busy: Option<String>,
}

impl State {
    fn connected(&self) -> Option<&(BdAddr, String)> {
        self.conn.as_ref().filter(|c| c.is_connected()).and_then(|c| c.peer.as_ref())
    }

    fn protocol(&self) -> Option<String> {
        link_description(self.conn.as_ref()?, self.aptx_active).map(|l| format!("Protocol: {l}"))
    }

    fn headline(&self) -> String {
        if !self.present {
            return "No BT-W5 or BT-W6 found".into();
        }
        if let Some(b) = &self.busy {
            return b.clone();
        }
        if self.pairing {
            return "Pairing: put your headset in pairing mode".into();
        }
        match (self.connected(), &self.conn) {
            (Some((_, name)), _) => name.clone(),
            (None, Some(c)) if c.state == 0x06 => "Searching for paired headsets".into(),
            _ => "No headset connected".into(),
        }
    }

    /// LED-matching colour (RGB) and opacity for the icon.
    fn colour(&self) -> ([u8; 3], u8) {
        const GREY: [u8; 3] = [0x9a, 0x9a, 0x9a];
        if !self.present {
            return (GREY, 0x70);
        }
        if self.pairing {
            return ([0x3b, 0x82, 0xf6], 0xff);
        }
        if self.connected().is_none() {
            return (GREY, 0xb0);
        }
        if self.conn.as_ref().is_some_and(|c| c.in_call()) {
            return ([0xef, 0x44, 0x44], 0xff);
        }
        match self.aptx_active {
            Some(APTX_LOW_LATENCY) => ([0x14, 0xc8, 0xe6], 0xff),
            Some(APTX_HIGH_QUALITY) => ([0xa8, 0x55, 0xf7], 0xff),
            _ => ([0xd0, 0xd0, 0xd0], 0xff),
        }
    }
}

enum Cmd {
    SetMode(u8),
    SetHfp(bool),
    SetAutoHfp(bool),
    Connect(BdAddr, String),
    Disconnect,
    Pair,
    Refresh,
}

struct Tray {
    state: State,
    tx: Sender<Cmd>,
}

impl Tray {
    fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

impl ksni::Tray for Tray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "bt-w".into()
    }

    fn title(&self) -> String {
        self.state.model.unwrap_or("BT-W").into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let (rgb, alpha) = self.state.colour();
        [16, 22, 24, 32, 48, 64].into_iter().map(|s| headphones(s, rgb, alpha)).collect()
    }

    fn tool_tip(&self) -> ToolTip {
        let mut description = self.state.headline();
        if let Some(p) = self.state.protocol() {
            description.push('\n');
            description.push_str(&p);
        }
        if let Some(e) = &self.state.error {
            description.push_str(&format!("\nError: {e}"));
        }
        let title = format!("Creative {}", self.state.model.unwrap_or("BT-W5 / BT-W6"));
        ToolTip { title, description, ..Default::default() }
    }

    fn menu_about_to_show(&mut self) {
        self.send(Cmd::Refresh);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = &self.state;
        let mut items: Vec<MenuItem<Self>> = vec![label(s.headline())];
        if let Some(p) = s.protocol() {
            items.push(label(p));
        }
        if let Some(e) = &s.error {
            items.push(label(format!("Error: {e}")));
        }
        if !s.present {
            items.push(MenuItem::Separator);
            items.push(quit());
            return items;
        }

        let connected = s.connected().cloned();
        let idle = s.busy.is_none();

        // aptX Adaptive mode: only meaningful over a live aptX Adaptive link.
        items.push(MenuItem::Separator);
        let mode = s.aptx_active.or(s.aptx_set);
        items.push(
            RadioGroup {
                selected: if mode == Some(APTX_HIGH_QUALITY) { 1 } else { 0 },
                select: Box::new(|t: &mut Self, i| {
                    let m = if i == 1 { APTX_HIGH_QUALITY } else { APTX_LOW_LATENCY };
                    t.state.aptx_set = Some(m);
                    t.send(Cmd::SetMode(m));
                }),
                options: ["Low latency", "High quality"]
                    .into_iter()
                    .map(|l| RadioItem {
                        label: l.into(),
                        enabled: idle && connected.is_some() && s.aptx_active.is_some(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into(),
        );

        items.push(MenuItem::Separator);
        if let Some(hfp) = s.hfp {
            items.push(
                CheckmarkItem {
                    label: "Headset microphone (HFP)".into(),
                    checked: hfp,
                    enabled: idle,
                    activate: Box::new(move |t: &mut Self| t.send(Cmd::SetHfp(!hfp))),
                    ..Default::default()
                }
                .into(),
            );
        }
        if let Some(auto) = s.auto_hfp {
            items.push(
                CheckmarkItem {
                    label: "Switch to HFP when the mic is in use".into(),
                    checked: auto,
                    enabled: idle,
                    activate: Box::new(move |t: &mut Self| t.send(Cmd::SetAutoHfp(!auto))),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        for slot in s.paired.iter().filter(|p| !p.is_empty()) {
            let (addr, name) = (slot.addr, slot.name.clone());
            let is_current = connected.as_ref().is_some_and(|(a, _)| *a == addr);
            items.push(
                CheckmarkItem {
                    label: name.clone(),
                    checked: is_current,
                    enabled: idle,
                    activate: Box::new(move |t: &mut Self| {
                        if !is_current {
                            t.send(Cmd::Connect(addr, name.clone()));
                        }
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        if s.pairing {
            if s.found.is_empty() {
                items.push(label("Looking for headsets…".into()));
            }
            for r in &s.found {
                let (addr, name) = (r.addr, r.name.clone());
                let shown = if name.is_empty() { addr.to_string() } else { name.clone() };
                items.push(
                    StandardItem {
                        label: format!("Pair with {shown}"),
                        icon_name: "list-add".into(),
                        enabled: idle,
                        activate: Box::new(move |t: &mut Self| t.send(Cmd::Connect(addr, name.clone()))),
                        ..Default::default()
                    }
                    .into(),
                );
            }
        } else {
            items.push(
                StandardItem {
                    label: "Pair new headset…".into(),
                    icon_name: "list-add".into(),
                    enabled: idle,
                    activate: Box::new(|t: &mut Self| t.send(Cmd::Pair)),
                    ..Default::default()
                }
                .into(),
            );
        }
        if connected.is_some() {
            items.push(
                StandardItem {
                    label: "Disconnect".into(),
                    icon_name: "network-disconnect".into(),
                    enabled: idle,
                    activate: Box::new(|t: &mut Self| t.send(Cmd::Disconnect)),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        items.push(quit());
        items
    }
}

fn label(text: String) -> MenuItem<Tray> {
    StandardItem { label: text, enabled: false, ..Default::default() }.into()
}

fn quit() -> MenuItem<Tray> {
    StandardItem {
        label: "Quit".into(),
        icon_name: "application-exit".into(),
        activate: Box::new(|_| std::process::exit(0)),
        ..Default::default()
    }
    .into()
}

/// Headphones glyph, anti-aliased, as ARGB32 (network byte order).
fn headphones(size: i32, rgb: [u8; 3], alpha: u8) -> Icon {
    fn rounded_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
        let cx = x.clamp(x0 + r, x1 - r);
        let cy = y.clamp(y0 + r, y1 - r);
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r
    }
    fn inside(x: f32, y: f32) -> bool {
        // Headband: upper half of a ring, continuing straight down into the cups.
        let (cx, cy) = (0.5, 0.52);
        let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        let band = (y <= cy && (0.31..=0.41).contains(&d))
            || (y > cy && y < 0.66 && ((0.09..=0.19).contains(&x) || (0.81..=0.91).contains(&x)));
        let cups = rounded_rect(x, y, 0.05, 0.54, 0.31, 0.94, 0.09)
            || rounded_rect(x, y, 0.69, 0.54, 0.95, 0.94, 0.09);
        band || cups
    }
    const SS: i32 = 4;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    hits += inside(x, y) as u32;
                }
            }
            let a = (hits * alpha as u32 / (SS * SS) as u32) as u8;
            data.extend_from_slice(&[a, rgb[0], rgb[1], rgb[2]]);
        }
    }
    Icon { width: size, height: size, data }
}

pub fn run() -> Result<()> {
    let _lock = single_instance()?;
    let (tx, rx) = mpsc::channel();
    let handle = Tray { state: State::default(), tx }
        .assume_sni_available(true)
        .spawn()
        .context("starting the tray icon")?;
    worker(handle, rx)
}

fn single_instance() -> Result<File> {
    let dir: std::path::PathBuf =
        std::env::var_os("XDG_RUNTIME_DIR").map(Into::into).unwrap_or_else(std::env::temp_dir);
    let path = dir.join("bt-w-tray.lock");
    let f = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    if f.try_lock().is_err() {
        bail!("the bt-w tray is already running");
    }
    Ok(f)
}

fn publish(handle: &Handle<Tray>, st: &State) {
    if handle.update(|t| t.state = st.clone()).is_none() {
        std::process::exit(0); // tray service is gone
    }
}

fn worker(handle: Handle<Tray>, rx: Receiver<Cmd>) -> ! {
    let mut last_error = None;
    loop {
        match Dongle::open() {
            Ok(d) => {
                if let Err(e) = session(&d, &handle, &rx) {
                    last_error = Some(format!("{e:#}"));
                }
            }
            Err(e) => {
                let missing = State { present: false, error: Some(format!("{e:#}")), ..Default::default() };
                publish(&handle, &missing);
                last_error = None;
            }
        }
        if let Some(e) = last_error.take() {
            eprintln!("bt-w tray: {e}; reopening");
        }
        // Menu clicks while the dongle is away do nothing.
        while rx.try_recv().is_ok() {}
        thread::sleep(Duration::from_secs(2));
    }
}

fn refresh(d: &Dongle, st: &mut State) -> Result<()> {
    st.present = true;
    st.model = Some(d.model);
    st.conn = Some(d.connection()?);
    st.aptx_set = Some(d.get(Setting::AptxMode)?);
    st.aptx_active = d.active_mode();
    st.hfp = Some(d.get(Setting::Hfp)? == 1);
    st.auto_hfp = Some(d.get(Setting::AutoHfp)? == 1);
    st.paired = d.paired()?;
    if st.connected().is_some() {
        st.pairing = false;
        st.found.clear();
    }
    Ok(())
}

/// Talk to one opened dongle until it goes away (unplugged, or restarting after an
/// HFP change).
fn session(d: &Dongle, handle: &Handle<Tray>, rx: &Receiver<Cmd>) -> Result<()> {
    let mut st = State::default();
    refresh(d, &mut st)?;
    publish(handle, &st);

    let mut dirty: Option<Instant> = None;
    let mut last_refresh = Instant::now();
    let mut busy_until: Option<Instant> = None;

    loop {
        while let Ok(cmd) = rx.try_recv() {
            let restarting = matches!(cmd, Cmd::SetHfp(_));
            st.error = None;
            match apply(d, &mut st, cmd) {
                Ok(Some(busy)) => {
                    st.busy = Some(busy);
                    busy_until = Some(Instant::now() + Duration::from_secs(25));
                }
                Ok(None) => {}
                Err(e) => st.error = Some(format!("{e:#}")),
            }
            publish(handle, &st);
            if restarting && st.error.is_none() {
                // The dongle re-enumerates; let it go and reopen.
                thread::sleep(Duration::from_secs(3));
                return Ok(());
            }
            dirty = Some(Instant::now() + Duration::from_millis(300));
        }

        if let Some(f) = d.recv(Duration::from_millis(150))? {
            if let Some(r) = ScanResult::parse(&f) {
                if st.pairing && !st.found.iter().any(|x| x.addr == r.addr) {
                    st.found.push(r);
                    publish(handle, &st);
                }
            } else if f.ack().is_none() {
                if let Some(e) = ConnEvent::parse(&f) {
                    st.conn = Some(e);
                    if st.connected().is_some() {
                        st.busy = None;
                        busy_until = None;
                    }
                    publish(handle, &st);
                }
                // Events come in bursts (08, 0b, 09 03...): refresh once they settle.
                dirty = Some(Instant::now() + Duration::from_millis(400));
            }
        }

        if busy_until.is_some_and(|t| Instant::now() >= t) {
            busy_until = None;
            st.busy = None;
            st.error = Some("headset did not connect".into());
            publish(handle, &st);
        }

        let periodic = last_refresh.elapsed() > Duration::from_secs(30);
        if dirty.is_some_and(|t| Instant::now() >= t) || periodic {
            dirty = None;
            last_refresh = Instant::now();
            refresh(d, &mut st)?;
            publish(handle, &st);
        }
    }
}

/// Run a menu action. Returns a busy message for actions that finish asynchronously.
fn apply(d: &Dongle, st: &mut State, cmd: Cmd) -> Result<Option<String>> {
    match cmd {
        Cmd::SetMode(m) => d.set(Setting::AptxMode, m)?,
        Cmd::SetHfp(on) => {
            d.set(Setting::Hfp, if on { 1 } else { 2 })?;
            return Ok(Some("Restarting the dongle…".into()));
        }
        Cmd::SetAutoHfp(on) => d.set(Setting::AutoHfp, on as u8)?,
        Cmd::Disconnect => d.disconnect()?,
        Cmd::Pair => {
            d.disconnect()?;
            st.pairing = true;
            st.found.clear();
        }
        Cmd::Connect(addr, name) => {
            if st.connected().is_some() {
                // Switch headsets the way Creative's app does: drop the link first.
                d.disconnect()?;
                let settle = Instant::now() + Duration::from_millis(1500);
                while d.recv(settle.saturating_duration_since(Instant::now()))?.is_some() {}
            }
            d.connect(addr)?;
            let name = if name.is_empty() { addr.to_string() } else { name };
            return Ok(Some(format!("Connecting to {name}…")));
        }
        Cmd::Refresh => refresh(d, st)?,
    }
    Ok(None)
}
