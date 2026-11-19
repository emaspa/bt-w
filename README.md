# bt-w

Command-line tool and tray icon for the Creative BT-W5 and BT-W6 Bluetooth audio dongles: switch aptX Adaptive between low latency and high quality, toggle HFP, list and manage pairings, and pair new headsets. No Creative app needed.

It goes through the OS HID driver, hidraw on Linux, so the dongle's media keys keep working. The original Python script detaches that driver instead. The protocol comes from the USB capture in [RaphaelWimmer/btw5-switch](https://github.com/RaphaelWimmer/btw5-switch). I tested it on a BT-W5 running firmware 1.2.230511.1220. Everything except the HFP on/off switch has run against the real dongle. That switch follows the capture byte for byte.

## Supported dongles

| Dongle | USB ID | Status |
|---|---|---|
| BT-W5 | `041e:3130` | Tested, firmware 1.2.230511.1220 |
| BT-W6 | `041e:3132` | Untested. Another user reported that the original script's aptX switching works with this ID, so the commands are probably shared. Its aptX Lossless and LE Audio features have no known commands yet. |

If you have a BT-W6, the output of `bt-w -v status` tells whether its replies arrive.

## Install

```sh
cargo install --path .
```

On Linux, let your user open the device without root:

```sh
sudo install -m644 70-creative-bt-w.rules /etc/udev/rules.d/
sudo udevadm control --reload
sudo udevadm trigger --subsystem-match=hidraw --action=change
```

## Tray icon

`bt-w tray` puts a headphones icon in the system tray. Its colour follows the dongle's LED: cyan for low latency, purple for high quality, blue while pairing, grey with no headset connected. Click it for a menu with the aptX mode, the HFP switches, your paired headsets (click one to switch to it), "Pair new headset…" and Disconnect.

The icon updates as soon as the dongle reports a change, including presses of its own button and headsets turning off. It needs a panel that supports StatusNotifierItem, which covers KDE Plasma and most others. GNOME needs the AppIndicator extension. The tray is Linux only, while the CLI also builds on macOS and Windows.

To start it with your desktop:

```sh
sed "s|^Exec=bt-w|Exec=$HOME/.cargo/bin/bt-w|" bt-w-tray.desktop > ~/.config/autostart/bt-w-tray.desktop
```

## Usage

```
$ bt-w
Model:       Creative BT-W5
Firmware:    1.2.230511.1220
Serial:      YDSA0180000000000A
Address:     04:3C:21:AA:BB:CC
Headset:     Aurvana Ace 2 (02:00:55:66:77:88)
Protocol:    A2DP, aptX Adaptive low latency
aptX mode:   low latency
HFP:         allowed
Auto HFP:    off
Paired:      2 of 4 slots
  [0] 02:00:11:22:33:44 Aurvana Ace
  [1] 02:00:55:66:77:88 Aurvana Ace 2
```

| Command | What it does |
|---|---|
| `bt-w mode ll` / `bt-w mode hq` | aptX Adaptive mode. The LED turns cyan for low latency and purple for high quality. Needs a connected headset. |
| `bt-w hfp on` / `off` | Hands-free profile, which enables the headset mic. The dongle restarts. |
| `bt-w auto-hfp on` / `off` | Switch to HFP when an app opens the mic. |
| `bt-w paired` | List paired headsets. |
| `bt-w scan [--secs 30] [--connect NAME\|ADDR]` | Disconnect, enter pairing mode and list headsets in range. `--connect` pairs with one as soon as it shows up. |
| `bt-w connect ADDR` | Connect to a headset. |
| `bt-w disconnect` | Drop the headset; the dongle enters pairing mode. |
| `bt-w forget-all --yes` | Erase all pairings. The dongle restarts. There is no known command to forget a single headset. |
| `bt-w tray` | Run the tray icon. |
| `bt-w monitor` | Print every message the dongle sends. |
| `bt-w raw 6b 09 08` | Send a raw frame and print the replies. |

Add `-v` to any command to see the frames on the wire.

Headset volume is plain USB Audio, so use your system mixer for it.

The headset mic only carries sound in call mode (HFP, red LED). In music mode the BT-W5 mic input records digital silence. You enter call mode by double-pressing the dongle's button, or automatically with `auto-hfp on`, which switches when an app starts recording. Auto HFP reacts only to the moment recording starts. If a program keeps the mic open all the time, such as a mixer with a permanent input channel, the switch never happens.

## Protocol

All traffic uses HID report ID 3 on interface 0, usage page `0xff01`, with 64 bytes each way.

```
03 5a <cmd> <len> <payload[len]>   zero padded
```

The firmware reuses its transmit buffer, so bytes after `len` can be left over from earlier messages. Addresses go least significant byte first.

| Request | Reply | Meaning |
|---|---|---|
| `09 01 02` | `09 12 02 <n> <string>` | firmware version |
| `09 01 03` | `09 13 03 <string>` | serial number |
| `09 02 01 00` | `09 08 01 00 <addr>` | dongle's Bluetooth address |
| `6b 02 09 <id>` | `6b 03 09 <id> <value>` | read a setting |
| `6b 03 0a <id> <value>` | ack `02 0a 6b 00 ...` | write a setting |
| `6b 02 02 <slot>` | `6b 2d 02 <slot> 80 <addr> 1f <name[31]>` | paired-device slot |
| `6b 01 06` | `6b 29 06 <state> <addr> 20 <name[32]>` | connection state, also sent unprompted |
| `6b 02 04 01` | ack, then `6b 0d 05 <addr> <n> <name> <rssi i16>` per device found | disconnect and scan |
| `6b 08 04 02 <addr>` | ack, then connection events | connect |
| `6b 02 07 ff` | ack | forget all pairings |

Acks look like `02 0a <cmd> <status> <echo>`. Status `00` means success and `80` or `81` means rejected.

Each setting has a write ID, and its read ID is the write ID plus 5.

| Setting | Write ID | Read ID | Values |
|---|---|---|---|
| HFP | `01` | `06` | 1 on, 2 off |
| Auto HFP | `02` | `07` | 1 on, 0 off |
| aptX Adaptive mode | `03` | `08` | `20` low latency, `40` high quality |

Read ID `01` returns the number of pairing slots, 4 on this unit, and `6b 01` returns how many are in use. Read ID `03` returns the aptX mode running on the current link, and the dongle rejects it when nothing is connected. I don't know what `00`, `02`, `04` and `05` mean.

The dongle has no codec query. `Protocol:` is derived from the connection state and read ID `03`. Call mode means HFP. In music mode, a value in `03` means aptX Adaptive. Without one, the codec is aptX HD, aptX or SBC, and the tool can't tell which.

Connection state `06` means paging. The dongle cycles through its paired headsets trying to reconnect, and the LED blinks white. A headset that connects produces `08`, followed by `0b` in music mode (A2DP) or `0c` in call mode (HFP, LED red). A disconnect produces `07`, then `0a` as the dongle starts pairing mode.

If you write the aptX mode while no headset is connected, the dongle acks it and then falls back to high quality. `bt-w mode` refuses to run in that state for this reason.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
