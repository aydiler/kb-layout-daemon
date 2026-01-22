use evdev::{uinput::VirtualDeviceBuilder, AttributeSet, Device, EventType, InputEvent, InputEventKind, Key, MiscType, RelativeAxisType};
use futures::StreamExt;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::sync::watch;
use tokio_udev::{AsyncMonitorSocket, MonitorBuilder};
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

// Track active keyboard monitors for hot-plug support
struct KeyboardMonitor {
    #[allow(dead_code)] // May be used for graceful shutdown in the future
    handle: JoinHandle<()>,
    shutdown_tx: watch::Sender<bool>,
}

type ActiveMonitors = Arc<std::sync::Mutex<HashMap<PathBuf, KeyboardMonitor>>>;

// Check if a device matches any configured keyboard
fn match_keyboard_config<'a>(device: &Device, config: &'a Config) -> Option<&'a KeyboardConfig> {
    let name = device.name().unwrap_or("Unknown");

    if !device.supported_events().contains(EventType::KEY) {
        return None;
    }

    config.keyboards.iter().find(|kb| {
        name.to_lowercase().contains(&kb.name.to_lowercase())
    })
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

/// Switch layout and wait for KDE to confirm the change.
/// Polls getLayout() until it matches the target, with a timeout.
fn switch_layout_confirmed(conn: &Connection, layout_index: u32) -> Result<(), zbus::Error> {
    switch_layout(conn, layout_index)?;

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(50) {
        if let Ok(current) = get_current_layout(conn) {
            if current == layout_index {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_micros(100));
    }

    // Timeout reached - proceed anyway, layout was set
    warn!("Layout switch confirmation timeout - proceeding");
    Ok(())
}

/// Emit events to virtual keyboard.
/// Events from the physical keyboard already include SYN_REPORT markers,
/// so we forward them as-is without adding extra synchronization events.
fn emit_event_batch(
    vk: &mut evdev::uinput::VirtualDevice,
    events: &[InputEvent],
) -> Result<(), std::io::Error> {
    if events.is_empty() {
        return Ok(());
    }
    vk.emit(events)
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
    shutdown_rx: watch::Receiver<bool>,
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
    // Track actually pressed keys to avoid releasing unpressed keys (especially Meta)
    let mut pressed_keys: HashSet<u16> = HashSet::new();

    loop {
        // Check for shutdown signal
        if *shutdown_rx.borrow() {
            info!("Shutdown signal received for '{}', stopping monitor", name);
            break;
        }

        let is_grab_mode = GRAB_MODE.load(Ordering::SeqCst);

        // Handle mode changes - need to re-open device with different grab state
        if device.is_none() || is_grab_mode != was_grab_mode {
            if is_grab_mode != was_grab_mode {
                info!(
                    "'{}' mode changing from {} to {}",
                    name,
                    if was_grab_mode { "GRAB" } else { "PASSIVE" },
                    if is_grab_mode { "GRAB" } else { "PASSIVE" }
                );
            }
            // Release only actually pressed keys before switching
            // This avoids sending spurious Meta key releases that trigger KDE launcher
            if device.is_some() && was_grab_mode && !pressed_keys.is_empty() {
                info!(
                    "'{}' releasing {} pressed keys before mode switch: {:?}",
                    name,
                    pressed_keys.len(),
                    pressed_keys
                );
                let release_events: Vec<InputEvent> = pressed_keys
                    .iter()
                    .map(|&code| InputEvent::new(EventType::KEY, code, 0))
                    .collect();
                if let Err(e) = emit_event_batch(&mut virtual_kb, &release_events) {
                    warn!("Failed to release keys during mode switch: {}", e);
                }
                pressed_keys.clear();
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
        let events_result: Result<Vec<InputEvent>, std::io::Error> = {
            let dev = device.as_mut().unwrap();
            dev.fetch_events().map(|iter| iter.collect())
        };

        let events = match events_result {
            Ok(e) if !e.is_empty() => e,
            Ok(_) => continue, // Empty events, loop again
            Err(e) => {
                // Check if it's a real disconnection or a recoverable error
                if e.raw_os_error() == Some(19) {
                    // ENODEV - device actually disconnected
                    info!("Device '{}' disconnected (ENODEV), stopping monitor", name);
                    break;
                } else if e.raw_os_error() == Some(11) {
                    // EAGAIN - would block, just continue (shouldn't happen with blocking read)
                    continue;
                } else {
                    // Log other errors and try to recover by re-opening device
                    warn!(
                        "Error reading from '{}': {} (os error: {:?}), re-opening device",
                        name,
                        e,
                        e.raw_os_error()
                    );
                    device = None; // Force device re-open on next iteration
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        };

        // Check if we need to switch layout (on key press) and track pressed keys
        let current = CURRENT_LAYOUT.load(Ordering::SeqCst);
        let mut need_switch = false;

        for ev in &events {
            if let InputEventKind::Key(key) = ev.kind() {
                match ev.value() {
                    1 => {
                        // Key press
                        pressed_keys.insert(key.code());
                        if current != layout_index {
                            need_switch = true;
                        }
                    }
                    0 => {
                        // Key release
                        pressed_keys.remove(&key.code());
                    }
                    _ => {} // Key repeat (value=2) - ignore for tracking
                }
            }
        }

        // Sanity check: warn if too many keys are tracked as pressed (possible state corruption)
        if pressed_keys.len() > 10 {
            warn!(
                "'{}' has {} keys tracked as pressed (possible state issue): {:?}",
                name,
                pressed_keys.len(),
                pressed_keys
            );
        }

        // Switch layout before forwarding events
        if need_switch {
            let mode_str = if is_grab_mode { "Grab" } else { "Passive" };
            info!(
                "[{}] Switching layout to {} (index {}) - input from '{}'",
                mode_str, layout_name, layout_index, name
            );

            // Use confirmed switch to wait for KDE to apply the layout
            if let Err(e) = switch_layout_confirmed(&dbus_conn, layout_index) {
                error!("Failed to switch layout: {}", e);
            }
        }

        // Forward events in grab mode
        if is_grab_mode {
            if let Err(e) = emit_event_batch(&mut virtual_kb, &events) {
                error!("Failed to emit events for '{}': {}", name, e);
                // Try to recover by recreating the virtual keyboard
                warn!("Attempting to recreate virtual keyboard for '{}'", name);
                match create_virtual_keyboard() {
                    Ok(new_vk) => {
                        virtual_kb = new_vk;
                        info!("Successfully recreated virtual keyboard for '{}'", name);
                        // Retry emitting events with new virtual keyboard
                        if let Err(e2) = emit_event_batch(&mut virtual_kb, &events) {
                            error!("Still failed to emit events after recreating vk: {}", e2);
                        }
                    }
                    Err(e2) => {
                        error!("Failed to recreate virtual keyboard: {}", e2);
                    }
                }
            }
        }
    }
}

// Spawn a keyboard monitor thread with shutdown signaling
fn spawn_keyboard_monitor(
    path: PathBuf,
    name: String,
    layout_index: u32,
    layout_name: String,
    dbus_conn: Arc<Connection>,
    monitors: &ActiveMonitors,
) {
    let mut monitors_guard = monitors.lock().unwrap();

    // Don't spawn if already monitoring this path
    if monitors_guard.contains_key(&path) {
        return;
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let path_clone = path.clone();

    let handle = thread::spawn(move || {
        monitor_keyboard(path_clone, name, layout_index, layout_name, dbus_conn, shutdown_rx);
    });

    monitors_guard.insert(
        path,
        KeyboardMonitor {
            handle,
            shutdown_tx,
        },
    );
}

// Stop a keyboard monitor
fn stop_keyboard_monitor(path: &PathBuf, monitors: &ActiveMonitors) {
    let mut monitors_guard = monitors.lock().unwrap();

    if let Some(monitor) = monitors_guard.remove(path) {
        // Signal shutdown
        let _ = monitor.shutdown_tx.send(true);
        // Don't wait for thread - it will exit on its own
    }
}

// Udev monitor for hot-plug detection
async fn run_udev_monitor(config: Arc<Config>, dbus_conn: Arc<Connection>, monitors: ActiveMonitors) {
    let builder = match MonitorBuilder::new() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to create udev monitor builder: {}", e);
            return;
        }
    };

    let builder = match builder.match_subsystem("input") {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to set subsystem filter: {}", e);
            return;
        }
    };

    let socket = match builder.listen() {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to start udev listener: {}", e);
            return;
        }
    };

    let mut async_monitor = match AsyncMonitorSocket::new(socket) {
        Ok(m) => m,
        Err(e) => {
            error!("Failed to create async monitor: {}", e);
            return;
        }
    };

    info!("Udev monitor started - hot-plug detection enabled");

    while let Some(event) = async_monitor.next().await {
        let event = match event {
            Ok(e) => e,
            Err(e) => {
                warn!("Udev event error: {}", e);
                continue;
            }
        };

        let devnode = match event.devnode() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };

        // Only handle /dev/input/event* devices
        if !devnode.to_string_lossy().contains("/dev/input/event") {
            continue;
        }

        match event.event_type() {
            tokio_udev::EventType::Add | tokio_udev::EventType::Bind => {
                // Small delay to let device settle
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Try to open and check if it matches config
                if let Ok(device) = Device::open(&devnode) {
                    if let Some(kb_config) = match_keyboard_config(&device, &config) {
                        let name = device.name().unwrap_or("Unknown").to_string();
                        info!(
                            "Hot-plug: Found keyboard '{}' at {:?} -> {} (index {})",
                            name, devnode, kb_config.layout_name, kb_config.layout_index
                        );
                        spawn_keyboard_monitor(
                            devnode,
                            name,
                            kb_config.layout_index,
                            kb_config.layout_name.clone(),
                            Arc::clone(&dbus_conn),
                            &monitors,
                        );
                    }
                }
            }
            tokio_udev::EventType::Remove | tokio_udev::EventType::Unbind => {
                // Check if we were monitoring this device
                let was_monitored = {
                    let guard = monitors.lock().unwrap();
                    guard.contains_key(&devnode)
                };

                if was_monitored {
                    info!("Hot-plug: Device removed at {:?}", devnode);
                    stop_keyboard_monitor(&devnode, &monitors);
                }
            }
            _ => {}
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

    let config = Arc::new(load_config());
    info!("Configuration: {:?}", *config);

    // Set initial mode
    let initial_grab = config.mode.to_lowercase() != "passive";
    GRAB_MODE.store(initial_grab, Ordering::SeqCst);
    info!(
        "Initial mode: {}",
        if initial_grab { "grab" } else { "passive" }
    );

    // Set up D-Bus connection for layout switching
    let dbus_conn = Arc::new(Connection::session()?);
    let current = get_current_layout(&dbus_conn).unwrap_or(0);
    CURRENT_LAYOUT.store(current, Ordering::SeqCst);
    info!("Current layout index: {}", current);

    // Shared state for active keyboard monitors (for hot-plug support)
    let monitors: ActiveMonitors = Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Find and start monitoring initially connected keyboards
    let keyboards = find_keyboards(&config);

    if keyboards.is_empty() {
        warn!("No configured keyboards found at startup.");
        warn!("Available input devices:");
        for entry in std::fs::read_dir("/dev/input")?.flatten() {
            let path = entry.path();
            if path.to_string_lossy().contains("event") {
                if let Ok(device) = Device::open(&path) {
                    if device.supported_events().contains(EventType::KEY) {
                        warn!("  {:?}: {}", path, device.name().unwrap_or("Unknown"));
                    }
                }
            }
        }
        warn!("Hot-plug detection is active - connect a configured keyboard.");
    } else {
        // Spawn monitors for initially connected keyboards
        for (path, (name, layout_index, layout_name)) in keyboards {
            spawn_keyboard_monitor(
                path,
                name,
                layout_index,
                layout_name,
                Arc::clone(&dbus_conn),
                &monitors,
            );
        }
    }

    // Start D-Bus service and udev monitor in async runtime
    let config_for_udev = Arc::clone(&config);
    let dbus_for_udev = Arc::clone(&dbus_conn);
    let monitors_for_udev = Arc::clone(&monitors);

    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            // Start D-Bus service
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

            // Run udev monitor (this runs forever)
            run_udev_monitor(config_for_udev, dbus_for_udev, monitors_for_udev).await;
        });
    });

    // Give D-Bus service time to start
    thread::sleep(Duration::from_millis(100));

    info!("Monitoring keyboards... Press Ctrl+C to stop.");
    info!("Toggle mode: dbus-send --session --print-reply --dest=org.kblayout.Daemon /org/kblayout/Daemon org.kblayout.Daemon.ToggleMode");

    // Keep main thread alive
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
