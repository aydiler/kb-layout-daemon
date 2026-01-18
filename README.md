# kb-layout-daemon

A lightweight daemon that automatically switches keyboard layout based on which physical keyboard you're typing on.

Perfect for users with multiple keyboards who want different layouts for each (e.g., English on one keyboard, German on another).

## Features

- Monitors multiple keyboards simultaneously
- Switches layout on first keypress (~50-100ms latency)
- Native D-Bus integration with KDE Plasma
- Minimal resource usage (~2MB RAM, <1% CPU)
- Single static binary, no runtime dependencies
- Configurable via TOML file
- Systemd user service included

## Requirements

- Linux with evdev support
- KDE Plasma (Wayland or X11)
- User must be in the `input` group

## Installation

### From source

```bash
git clone https://github.com/aydiler/kb-layout-daemon
cd kb-layout-daemon
cargo build --release
sudo cp target/release/kb-layout-daemon /usr/local/bin/
```

### Arch Linux (AUR)

```bash
yay -S kb-layout-daemon
```

### From crates.io

```bash
cargo install kb-layout-daemon
```

## Setup

1. Add yourself to the `input` group:
   ```bash
   sudo usermod -aG input $USER
   ```
   Then log out and back in.

2. Create config file at `~/.config/kb-layout-daemon/config.toml`:
   ```toml
   [[keyboards]]
   name = "Lofree"
   layout_index = 1
   layout_name = "English (US)"

   [[keyboards]]
   name = "CHERRY"
   layout_index = 0
   layout_name = "German"
   ```

   The `layout_index` corresponds to the order in KDE's keyboard layout settings (0-based).

3. Install the systemd service:
   ```bash
   mkdir -p ~/.config/systemd/user
   cp kb-layout-daemon.service ~/.config/systemd/user/
   systemctl --user enable --now kb-layout-daemon
   ```

## Configuration

The config file uses TOML format. Each `[[keyboards]]` section defines a keyboard to monitor:

| Field | Description |
|-------|-------------|
| `name` | Substring to match in the device name (case-insensitive) |
| `layout_index` | KDE layout index (0-based, matches order in System Settings) |
| `layout_name` | Human-readable name for logging |

To find your keyboard names:
```bash
cat /proc/bus/input/devices | grep -A 4 "Name="
```

## How It Works

1. On startup, scans `/dev/input/event*` for keyboards matching configured names
2. Uses async I/O (tokio + evdev) to monitor all keyboards concurrently
3. On keypress, checks if layout switch is needed
4. Switches layout via D-Bus call to `org.kde.keyboard`

## Troubleshooting

**"No keyboards found"**
- Ensure you're in the `input` group: `groups | grep input`
- Log out and back in after adding yourself to the group

**Layout not switching**
- Check KDE has multiple layouts configured
- Verify `layout_index` matches your KDE layout order
- Check logs: `journalctl --user -u kb-layout-daemon -f`

## License

MIT
