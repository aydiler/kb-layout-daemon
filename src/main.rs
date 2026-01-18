use evdev::{Device, EventType, InputEventKind};
use futures::stream::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use zbus::Connection;

#[derive(Debug, Deserialize)]
struct Config {
    keyboards: Vec<KeyboardConfig>,
}

#[derive(Debug, Deserialize)]
struct KeyboardConfig {
    /// Name pattern to match (substring match)
    name: String,
    /// Layout index in KDE (0-based, matches order in kxkbrc LayoutList)
    layout_index: u32,
    /// Human-readable layout name for logging
    layout_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            keyboards: vec![
                KeyboardConfig {
                    name: "Lofree".to_string(),
                    layout_index: 1, // us is second in "de,us"
                    layout_name: "English (US)".to_string(),
                },
                KeyboardConfig {
                    name: "CHERRY".to_string(),
                    layout_index: 0, // de is first in "de,us"
                    layout_name: "German".to_string(),
                },
            ],
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

    // Enumerate all input devices
    for entry in std::fs::read_dir("/dev/input").unwrap().flatten() {
        let path = entry.path();
        if !path.to_string_lossy().contains("event") {
            continue;
        }

        if let Ok(device) = Device::open(&path) {
            let name = device.name().unwrap_or("Unknown");

            // Check if device supports key events (is a keyboard)
            if !device.supported_events().contains(EventType::KEY) {
                continue;
            }

            // Match against configured keyboards
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

async fn monitor_keyboard(
    path: PathBuf,
    name: String,
    layout_index: u32,
    layout_name: String,
    conn: Arc<Connection>,
    current_layout: Arc<Mutex<u32>>,
) {
    info!("Starting monitor for '{}' at {:?}", name, path);

    loop {
        let device = match Device::open(&path) {
            Ok(d) => d,
            Err(e) => {
                warn!("Failed to open {:?}: {}, retrying in 5s", path, e);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        let mut stream = match device.into_event_stream() {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to create event stream for {:?}: {}", path, e);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        while let Some(event) = stream.next().await {
            match event {
                Ok(ev) => {
                    // Only react to key press events (value 1 = press, 0 = release, 2 = repeat)
                    if let InputEventKind::Key(_) = ev.kind() {
                        if ev.value() == 1 {
                            let mut current = current_layout.lock().await;

                            if *current != layout_index {
                                info!(
                                    "Switching layout to {} (index {}) - input from '{}'",
                                    layout_name, layout_index, name
                                );

                                match switch_layout(&conn, layout_index).await {
                                    Ok(()) => {
                                        *current = layout_index;
                                    }
                                    Err(e) => {
                                        error!("Failed to switch layout: {}", e);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Error reading events from '{}': {}", name, e);
                    break;
                }
            }
        }

        warn!("Event stream ended for '{}', reconnecting...", name);
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    info!("kb-layout-daemon starting...");

    // Load configuration
    let config = load_config();
    info!("Configuration: {:?}", config);

    // Find keyboards
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

    // Connect to D-Bus session bus
    let conn = Arc::new(Connection::session().await?);

    // Get current layout
    let current = get_current_layout(&conn).await.unwrap_or(0);
    info!("Current layout index: {}", current);
    let current_layout = Arc::new(Mutex::new(current));

    // Spawn monitor tasks for each keyboard
    let mut handles = vec![];

    for (path, (name, layout_index, layout_name)) in keyboards {
        let conn = Arc::clone(&conn);
        let current_layout = Arc::clone(&current_layout);

        let handle = tokio::spawn(async move {
            monitor_keyboard(path, name, layout_index, layout_name, conn, current_layout).await;
        });

        handles.push(handle);
    }

    info!("Monitoring keyboards... Press Ctrl+C to stop.");

    // Wait for all tasks (they run forever unless interrupted)
    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}
