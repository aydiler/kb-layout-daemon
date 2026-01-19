use evdev::{uinput::VirtualDeviceBuilder, AttributeSet, Device, EventType, InputEvent, InputEventKind, Key, MiscType, RelativeAxisType};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};
use zbus::{blocking::Connection, interface};

// Mode: true = Grab (correct first key), false = Passive (zero latency)
static GRAB_MODE: AtomicBool = AtomicBool::new(true);
static CURRENT_LAYOUT: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Deserialize)]
struct Config {
    keyboards: Vec<KeyboardConfig>,
    #[serde(default = "default_mode")]
    mode: String,
}

fn default_mode() -> String {
    "grab".to_string()
}

#[derive(Debug, Deserialize)]
struct KeyboardConfig {
    name: String,
    layout_index: u32,
    layout_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            keyboards: vec![
                KeyboardConfig {
                    name: "Lofree".to_string(),
                    layout_index: 1,
                    layout_name: "English (US)".to_string(),
                },
                KeyboardConfig {
                    name: "CHERRY".to_string(),
                    layout_index: 0,
                    layout_name: "German".to_string(),
                },
            ],
            mode: "grab".to_string(),
        }
    }
}

fn load_config() -> Config {
    let config_path = dirs::config_dir()
        .map(|p| p.join("kb-layout-daemon").join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(config) => {
                    info!("Loaded config from {:?}", config_path);
                    return config;
                }
                Err(e) => {
                    warn!("Failed to parse config: {}, using defaults", e);
                }
            },
            Err(e) => {
                warn!("Failed to read config: {}, using defaults", e);
            }
        }
    } else {
        info!("No config file found at {:?}, using defaults", config_path);
    }

    Config::default()
}

fn find_keyboards(config: &Config) -> HashMap<PathBuf, (String, u32, String)> {
    let mut keyboards = HashMap::new();

    for entry in std::fs::read_dir("/dev/input").unwrap().flatten() {
        let path = entry.path();
        if !path.to_string_lossy().contains("event") {
            continue;
        }

        if let Ok(device) = Device::open(&path) {
            let name = device.name().unwrap_or("Unknown");

            if !device.supported_events().contains(EventType::KEY) {
                continue;
            }

            for kb_config in &config.keyboards {
                if name.to_lowercase().contains(&kb_config.name.to_lowercase()) {
                    info!(
                        "Found keyboard '{}' at {:?} -> {} (index {})",
                        name, path, kb_config.layout_name, kb_config.layout_index
                    );
                    keyboards.insert(
                        path.clone(),
                        (
                            name.to_string(),
                            kb_config.layout_index,
                            kb_config.layout_name.clone(),
                        ),
                    );
                    break;
                }
            }
        }
    }

    keyboards
}

fn switch_layout(conn: &Connection, layout_index: u32) -> Result<(), zbus::Error> {
    let proxy = zbus::blocking::Proxy::new(
        conn,
        "org.kde.keyboard",
        "/Layouts",
        "org.kde.KeyboardLayouts",
    )?;

    let result: bool = proxy.call("setLayout", &(layout_index,))?;

    if result {
        CURRENT_LAYOUT.store(layout_index, Ordering::SeqCst);
        Ok(())
    } else {
        Err(zbus::Error::Failure("setLayout returned false".to_string()))
    }
}

fn get_current_layout(conn: &Connection) -> Result<u32, zbus::Error> {
    let proxy = zbus::blocking::Proxy::new(
        conn,
        "org.kde.keyboard",
        "/Layouts",
        "org.kde.KeyboardLayouts",
    )?;

    proxy.call("getLayout", &())
}

fn create_virtual_keyboard() -> Result<evdev::uinput::VirtualDevice, std::io::Error> {
    let mut keys = AttributeSet::<Key>::new();
    // Include all possible key codes (KEY_MAX is typically 767)
    for i in 0..768u16 {
        keys.insert(Key::new(i));
    }

    // Add MSC types (for scan codes)
    let mut misc = AttributeSet::<MiscType>::new();
    misc.insert(MiscType::MSC_SCAN);

    // Add relative axes (for keyboards with trackpads/scroll)
    let mut rel = AttributeSet::<RelativeAxisType>::new();
    rel.insert(RelativeAxisType::REL_X);
    rel.insert(RelativeAxisType::REL_Y);
    rel.insert(RelativeAxisType::REL_WHEEL);
    rel.insert(RelativeAxisType::REL_HWHEEL);
    rel.insert(RelativeAxisType::REL_WHEEL_HI_RES);
    rel.insert(RelativeAxisType::REL_HWHEEL_HI_RES);

    VirtualDeviceBuilder::new()?
        .name("kb-layout-daemon virtual keyboard")
        .with_keys(&keys)?
        .with_msc(&misc)?
        .with_relative_axes(&rel)?
        .build()
}

// D-Bus interface for controlling the daemon
struct DaemonControl;

#[interface(name = "org.kblayout.Daemon")]
impl DaemonControl {
    fn get_mode(&self) -> &str {
        if GRAB_MODE.load(Ordering::SeqCst) {
            "grab"
        } else {
            "passive"
        }
    }

    fn set_mode(&self, mode: &str) -> bool {
        match mode.to_lowercase().as_str() {
            "passive" => {
                GRAB_MODE.store(false, Ordering::SeqCst);
                info!("Mode set to: passive (zero latency, first key may be wrong)");
                true
            }
            "grab" => {
                GRAB_MODE.store(true, Ordering::SeqCst);
                info!("Mode set to: grab (correct first key)");
                true
            }
            _ => false,
        }
    }

    fn toggle_mode(&self) -> &str {
        let was_grab = GRAB_MODE.fetch_xor(true, Ordering::SeqCst);
        if was_grab {
            info!("Mode toggled to: passive");
            "passive"
        } else {
            info!("Mode toggled to: grab");
            "grab"
        }
    }
}

// Keyboard monitor - runs in its own thread with its own virtual keyboard
fn monitor_keyboard(
    path: PathBuf,
    name: String,
    layout_index: u32,
    layout_name: String,
    dbus_conn: Arc<Connection>,
) {
    info!("Starting monitor for '{}' at {:?}", name, path);

    // Create dedicated virtual keyboard for this physical keyboard
    let mut virtual_kb = match create_virtual_keyboard() {
        Ok(vk) => vk,
        Err(e) => {
            error!("Failed to create virtual keyboard for '{}': {}", name, e);
            return;
        }
    };

    let mut was_grab_mode = GRAB_MODE.load(Ordering::SeqCst);
    let mut device: Option<Device> = None;

    loop {
        let is_grab_mode = GRAB_MODE.load(Ordering::SeqCst);

        // Handle mode changes - need to re-open device with different grab state
        if device.is_none() || is_grab_mode != was_grab_mode {
            // Release any held keys before switching
            if let Some(ref mut dev) = device {
                if was_grab_mode {
                    // Send key release for all potentially pressed keys
                    let release_events: Vec<InputEvent> = (0..256u16)
                        .map(|code| InputEvent::new(EventType::KEY, code, 0))
                        .collect();
                    let _ = virtual_kb.emit(&release_events);
                    // Send SYN
                    let _ = virtual_kb.emit(&[InputEvent::new(EventType::SYNCHRONIZATION, 0, 0)]);
                }
                drop(dev);
            }
            device = None;

            // Open device
            let mut dev = match Device::open(&path) {
                Ok(d) => d,
                Err(e) => {
                    warn!("Failed to open {:?}: {}, retrying...", path, e);
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };

            // Grab if in grab mode
            if is_grab_mode {
                if let Err(e) = dev.grab() {
                    warn!("Failed to grab {:?}: {}, retrying...", path, e);
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            }

            device = Some(dev);
            was_grab_mode = is_grab_mode;
            info!(
                "'{}' now in {} mode",
                name,
                if is_grab_mode { "GRAB" } else { "PASSIVE" }
            );
        }

        // Read events in a block to limit borrow scope
        let events: Option<Vec<InputEvent>> = {
            let dev = device.as_mut().unwrap();
            match dev.fetch_events() {
                Ok(iter) => Some(iter.collect()),
                Err(_) => None,
            }
        };

        let events = match events {
            Some(e) if !e.is_empty() => e,
            Some(_) => continue,
            None => {
                // Device disconnected or error
                warn!("Error reading from '{}', reconnecting...", name);
                device = None;
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        // Check if we need to switch layout (on key press)
        let current = CURRENT_LAYOUT.load(Ordering::SeqCst);
        let mut need_switch = false;

        for ev in &events {
            if let InputEventKind::Key(_) = ev.kind() {
                if ev.value() == 1 && current != layout_index {
                    need_switch = true;
                    break;
                }
            }
        }

        // Switch layout before forwarding events
        if need_switch {
            let mode_str = if is_grab_mode { "Grab" } else { "Passive" };
            info!(
                "[{}] Switching layout to {} (index {}) - input from '{}'",
                mode_str, layout_name, layout_index, name
            );

            if let Err(e) = switch_layout(&dbus_conn, layout_index) {
                error!("Failed to switch layout: {}", e);
            } else if is_grab_mode {
                // Small delay to ensure layout is applied
                thread::sleep(Duration::from_micros(500));
            }
        }

        // Forward events in grab mode
        if is_grab_mode {
            if let Err(e) = virtual_kb.emit(&events) {
                error!("Failed to emit events: {}", e);
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    info!("kb-layout-daemon starting...");

    let config = load_config();
    info!("Configuration: {:?}", config);

    // Set initial mode
    let initial_grab = config.mode.to_lowercase() != "passive";
    GRAB_MODE.store(initial_grab, Ordering::SeqCst);
    info!(
        "Initial mode: {}",
        if initial_grab { "grab" } else { "passive" }
    );

    let keyboards = find_keyboards(&config);

    if keyboards.is_empty() {
        error!("No configured keyboards found! Check your config.");
        error!("Available input devices:");
        for entry in std::fs::read_dir("/dev/input")?.flatten() {
            let path = entry.path();
            if path.to_string_lossy().contains("event") {
                if let Ok(device) = Device::open(&path) {
                    if device.supported_events().contains(EventType::KEY) {
                        error!("  {:?}: {}", path, device.name().unwrap_or("Unknown"));
                    }
                }
            }
        }
        return Err("No keyboards found".into());
    }

    // Set up D-Bus connection for layout switching
    let dbus_conn = Arc::new(Connection::session()?);
    let current = get_current_layout(&dbus_conn).unwrap_or(0);
    CURRENT_LAYOUT.store(current, Ordering::SeqCst);
    info!("Current layout index: {}", current);

    // Start D-Bus service in separate thread
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let _conn = zbus::ConnectionBuilder::session()
                .unwrap()
                .name("org.kblayout.Daemon")
                .unwrap()
                .serve_at("/org/kblayout/Daemon", DaemonControl)
                .unwrap()
                .build()
                .await
                .unwrap();

            info!("D-Bus service started at org.kblayout.Daemon");

            // Keep the connection alive
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
            }
        });
    });

    // Give D-Bus service time to start
    thread::sleep(Duration::from_millis(100));

    info!("Monitoring keyboards... Press Ctrl+C to stop.");
    info!("Toggle mode: dbus-send --session --print-reply --dest=org.kblayout.Daemon /org/kblayout/Daemon org.kblayout.Daemon.ToggleMode");

    // Start keyboard monitors
    let mut handles = vec![];

    for (path, (name, layout_index, layout_name)) in keyboards {
        let dbus_conn = Arc::clone(&dbus_conn);

        let handle = thread::spawn(move || {
            monitor_keyboard(path, name, layout_index, layout_name, dbus_conn);
        });

        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        let _ = handle.join();
    }

    Ok(())
}
