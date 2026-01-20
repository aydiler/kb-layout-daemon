# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build Commands

```bash
cargo build --release          # Build optimized binary
cargo run                      # Run in debug mode
cargo clippy                   # Lint
cargo publish --allow-dirty    # Publish to crates.io
```

AUR package: `kb-layout-daemon-git`

## Testing the Daemon

The daemon requires the user to be in the `input` group. For testing without logout:
```bash
sg input -c "./target/release/kb-layout-daemon"
```

Control via D-Bus:
```bash
dbus-send --session --print-reply --dest=org.kblayout.Daemon /org/kblayout/Daemon org.kblayout.Daemon.GetMode
dbus-send --session --print-reply --dest=org.kblayout.Daemon /org/kblayout/Daemon org.kblayout.Daemon.ToggleMode
dbus-send --session --print-reply --dest=org.kblayout.Daemon /org/kblayout/Daemon org.kblayout.Daemon.SetMode string:"passive"
```

## Architecture

Single-binary daemon with one main.rs file (~445 lines). Key components:

**Global State (atomics)**
- `GRAB_MODE: AtomicBool` - Current mode (grab vs passive)
- `CURRENT_LAYOUT: AtomicU32` - Tracks active keyboard layout index

**Threading Model**
- Main thread: Initializes config, finds keyboards, spawns monitor threads
- One thread per physical keyboard: Runs `monitor_keyboard()` loop
- D-Bus service thread: Runs async tokio runtime for `org.kblayout.Daemon`

**Two Operating Modes**
- **Grab mode**: Exclusive device access via `EVIOCGRAB`, events forwarded through uinput virtual keyboard. Ensures correct layout on first keystroke (~1ms latency).
- **Passive mode**: No device grabbing, just monitors events. Zero latency but first key after switch may use old layout.

**Key Functions**
- `load_config()` - Reads `~/.config/kb-layout-daemon/config.toml`
- `find_keyboards()` - Scans `/dev/input/event*` matching config names
- `monitor_keyboard()` - Per-keyboard event loop (grab/read/forward)
- `create_virtual_keyboard()` - Creates uinput device with KEY, MSC_SCAN, and REL axes
- `switch_layout()` - D-Bus call to `org.kde.keyboard` to change layout

**Virtual Keyboard Requirements**
The virtual keyboard must include MSC_SCAN events and relative axes, otherwise some keys won't work in grab mode.

## Config Location

`~/.config/kb-layout-daemon/config.toml`

Find keyboard device names with:
```bash
cat /proc/bus/input/devices | grep -A 4 "Name="
```

## KDE Plasma Widget

Located in `widget/` directory. Install to `~/.local/share/plasma/plasmoids/org.kblayout.toggle/`. Uses `Plasma5Support.DataSource` with "executable" engine to run D-Bus commands.
