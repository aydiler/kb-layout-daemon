use evdev::{uinput::VirtualDeviceBuilder, AttributeSet, Device, EventType, InputEvent, InputEventKind, Key};
use futures::stream::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use zbus::{interface, Connection, ConnectionBuilder};

// Mode: true = Grab (correct first key), false = Passive (zero latency)
static GRAB_MODE: AtomicBool = AtomicBool::new(true);

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

async fn switch_layout(conn: &Connection, layout_index: u32) -> Result<(), zbus::Error> {
    let proxy = zbus::Proxy::new(
        conn,
        "org.kde.keyboard",
        "/Layouts",
        "org.kde.KeyboardLayouts",
    )
    .await?;

    let result: bool = proxy.call("setLayout", &(layout_index,)).await?;

    if result {
        Ok(())
    } else {
        Err(zbus::Error::Failure("setLayout returned false".to_string()))
    }
}

async fn get_current_layout(conn: &Connection) -> Result<u32, zbus::Error> {
    let proxy = zbus::Proxy::new(
        conn,
        "org.kde.keyboard",
        "/Layouts",
        "org.kde.KeyboardLayouts",
    )
    .await?;

    proxy.call("getLayout", &()).await
}

fn create_virtual_keyboard() -> Result<evdev::uinput::VirtualDevice, std::io::Error> {
    let mut keys = AttributeSet::<Key>::new();
    // Include all possible key codes (KEY_MAX is typically 767)
    for i in 0..768u16 {
        keys.insert(Key::new(i));
    }

    VirtualDeviceBuilder::new()?
        .name("kb-layout-daemon virtual keyboard")
        .with_keys(&keys)?
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

struct SharedState {
    current_layout: u32,
    dbus_conn: Connection,
    virtual_keyboard: evdev::uinput::VirtualDevice,
}

// Event reader thread for grab mode
fn read_events_blocking(path: PathBuf, tx: std::sync::mpsc::Sender<Vec<InputEvent>>, should_grab: Arc<AtomicBool>) {
    loop {
        let grab = should_grab.load(Ordering::SeqCst);

        let mut device = match Device::open(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to open {:?}: {}", path, e);
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };

        if grab {
            if let Err(e) = device.grab() {
                eprintln!("Failed to grab {:?}: {}", path, e);
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        }

        loop {
            // Check if mode changed
            let new_grab = should_grab.load(Ordering::SeqCst);
            if new_grab != grab {
                break; // Reconnect with new mode
            }

            match device.fetch_events() {
                Ok(events) => {
                    let events: Vec<InputEvent> = events.collect();
                    if tx.send(events).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("Error reading from {:?}: {}", path, e);
                    break;
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

async fn monitor_keyboard(
    path: PathBuf,
    name: String,
    layout_index: u32,
    layout_name: String,
    state: Arc<Mutex<SharedState>>,
) {
    info!("Starting monitor for '{}' at {:?}", name, path);

    let (tx, rx) = std::sync::mpsc::channel::<Vec<InputEvent>>();
    let rx = Arc::new(std::sync::Mutex::new(rx));
    let should_grab = Arc::new(AtomicBool::new(GRAB_MODE.load(Ordering::SeqCst)));

    let path_clone = path.clone();
    let should_grab_clone = Arc::clone(&should_grab);
    std::thread::spawn(move || {
        read_events_blocking(path_clone, tx, should_grab_clone);
    });

    loop {
        // Sync grab mode
        should_grab.store(GRAB_MODE.load(Ordering::SeqCst), Ordering::SeqCst);

        let rx_clone = Arc::clone(&rx);
        let events = tokio::task::spawn_blocking(move || {
            let rx = rx_clone.lock().unwrap();
            rx.recv_timeout(std::time::Duration::from_millis(100)).ok()
        })
        .await;

        let events = match events {
            Ok(Some(events)) => events,
            Ok(None) => continue,
            Err(_) => {
                warn!("Event reader task failed for '{}'", name);
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        let is_grab_mode = GRAB_MODE.load(Ordering::SeqCst);

        // First pass: check if we need to switch layout
        let mut need_switch = false;
        {
            let state = state.lock().await;
            for ev in &events {
                if let InputEventKind::Key(_) = ev.kind() {
                    if ev.value() == 1 && state.current_layout != layout_index {
                        need_switch = true;
                        break;
                    }
                }
            }
        }

        // Switch layout if needed (before forwarding events)
        if need_switch {
            let mut state = state.lock().await;
            if state.current_layout != layout_index {
                let mode_str = if is_grab_mode { "Grab" } else { "Passive" };
                info!(
                    "[{}] Switching layout to {} (index {}) - input from '{}'",
                    mode_str, layout_name, layout_index, name
                );

                match switch_layout(&state.dbus_conn, layout_index).await {
                    Ok(()) => {
                        state.current_layout = layout_index;
                        if is_grab_mode {
                            // Small delay to ensure layout is applied before forwarding
                            tokio::time::sleep(tokio::time::Duration::from_millis(2)).await;
                        }
                    }
                    Err(e) => {
                        error!("Failed to switch layout: {}", e);
                    }
                }
            }
        }

        // Forward all events in grab mode (outside the lock for better performance)
        if is_grab_mode {
            let mut state = state.lock().await;
            // Emit all events at once for better timing
            if let Err(e) = state.virtual_keyboard.emit(&events) {
                error!("Failed to emit events: {}", e);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    info!("Initial mode: {}", if initial_grab { "grab" } else { "passive" });

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

    // Create virtual keyboard for grab mode
    let virtual_keyboard = create_virtual_keyboard()?;
    info!("Created virtual keyboard for event forwarding");

    // Set up D-Bus service
    let _control_conn = ConnectionBuilder::session()?
        .name("org.kblayout.Daemon")?
        .serve_at("/org/kblayout/Daemon", DaemonControl)?
        .build()
        .await?;

    info!("D-Bus service started at org.kblayout.Daemon");

    let dbus_conn = Connection::session().await?;
    let current = get_current_layout(&dbus_conn).await.unwrap_or(0);
    info!("Current layout index: {}", current);

    let state = Arc::new(Mutex::new(SharedState {
        current_layout: current,
        dbus_conn,
        virtual_keyboard,
    }));

    let mut handles = vec![];

    for (path, (name, layout_index, layout_name)) in keyboards {
        let state = Arc::clone(&state);

        let handle = tokio::spawn(async move {
            monitor_keyboard(path, name, layout_index, layout_name, state).await;
        });

        handles.push(handle);
    }

    info!("Monitoring keyboards... Press Ctrl+C to stop.");
    info!("Toggle mode: dbus-send --session --print-reply --dest=org.kblayout.Daemon /org/kblayout/Daemon org.kblayout.Daemon.ToggleMode");

    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}
