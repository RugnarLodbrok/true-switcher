use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::input_events::{InputEventMessage, InputEventSender};
use crate::layout::Layout;
use zbus::blocking::{Connection, MessageIterator, Proxy, fdo::DBusProxy};
use zbus::{MatchRule, message::Type};

const SERVICE: &str = "org.kde.keyboard";
const PATH: &str = "/Layouts";
const INTERFACE: &str = "org.kde.KeyboardLayouts";
const RETRY_DELAY: Duration = Duration::from_secs(1);
const WARNING_INTERVAL: Duration = Duration::from_secs(60);
const CONFIG_FILE: &str = "kxkbrc";

#[cfg(test)]
mod tracking_tests;

#[derive(Clone)]
pub struct LayoutTracker {
    observation: Arc<Mutex<()>>,
    sender: InputEventSender,
}

impl LayoutTracker {
    pub fn start(sender: InputEventSender) -> Self {
        let tracker = Self::new(sender);
        let worker = tracker.clone();
        if let Err(error) = thread::Builder::new()
            .name("layout-tracker".into())
            .spawn(move || {
                let mut last_warning: Option<Instant> = None;
                loop {
                    let result = track(&worker, &mut || {
                        if last_warning.take().is_some() {
                            log::info!("true-switcher: layout tracking restored");
                        }
                    });
                    if let Err(error) = result {
                        let now = Instant::now();
                        if last_warning
                            .is_none_or(|last| now.duration_since(last) >= WARNING_INTERVAL)
                        {
                            log::warn!("true-switcher: layout tracking: {error}");
                            last_warning = Some(now);
                        }
                    }
                    thread::sleep(RETRY_DELAY);
                }
            })
        {
            log::error!("true-switcher: cannot start layout tracking: {error}");
        }
        tracker
    }

    fn new(sender: InputEventSender) -> Self {
        Self {
            observation: Arc::new(Mutex::new(())),
            sender,
        }
    }

    fn set(&self, layout: Option<Arc<Layout>>) {
        self.sender.send(InputEventMessage::LayoutChanged(layout));
    }

    fn observe(&self, read: impl FnOnce() -> io::Result<Arc<Layout>>) -> io::Result<()> {
        let _guard = self
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let result = read();
        self.set(result.as_ref().ok().cloned());
        result.map(|_| ())
    }

    fn invalidate(&self) {
        let _guard = self
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.set(None);
    }

    pub fn finish_correction(&self, result: io::Result<()>) {
        self.finish_with(result, current_layout);
    }

    fn finish_with(&self, result: io::Result<()>, read: impl FnOnce() -> io::Result<Arc<Layout>>) {
        let _guard = self
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let layout = read().ok();
        self.sender
            .send(InputEventMessage::CorrectionFinished { result, layout });
    }
}

fn track(tracker: &LayoutTracker, on_connected: &mut impl FnMut()) -> zbus::Result<()> {
    let connection = match Connection::session() {
        Ok(connection) => connection,
        Err(error) => {
            tracker.invalidate();
            return Err(error);
        }
    };
    track_connection(&connection, tracker, &read_configuration, on_connected)
}

fn subscribe(connection: &Connection) -> zbus::Result<MessageIterator> {
    let messages = MessageIterator::from(connection);
    let bus = DBusProxy::new(connection)?;
    bus.add_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(SERVICE)?
            .path(PATH)?
            .interface(INTERFACE)?
            .build(),
    )?;
    bus.add_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .add_arg(SERVICE)?
            .build(),
    )?;
    Ok(messages)
}

fn track_connection(
    connection: &Connection,
    tracker: &LayoutTracker,
    read_config: &impl Fn() -> io::Result<String>,
    on_connected: &mut impl FnMut(),
) -> zbus::Result<()> {
    let result = receive_changes(connection, tracker, read_config, on_connected);
    tracker.invalidate();
    result
}

fn receive_changes(
    connection: &Connection,
    tracker: &LayoutTracker,
    read_config: &impl Fn() -> io::Result<String>,
    on_connected: &mut impl FnMut(),
) -> zbus::Result<()> {
    let mut messages = subscribe(connection)?;
    let owner = DBusProxy::new(connection)?.get_name_owner(SERVICE.try_into()?)?;
    let proxy = Proxy::new(connection, owner.as_str(), PATH, INTERFACE)?;
    tracker
        .observe(|| query(&proxy, read_config))
        .map_err(zbus::Error::from)?;
    on_connected();
    for message in &mut messages {
        let message = message?;
        let header = message.header();
        if header.message_type() != Type::Signal {
            continue;
        }
        if header
            .interface()
            .is_some_and(|value| value.as_str() == "org.freedesktop.DBus")
            && header
                .member()
                .is_some_and(|value| value.as_str() == "NameOwnerChanged")
        {
            let (name, _, _): (String, String, String) = message.body().deserialize()?;
            if name == SERVICE {
                return Err(io::Error::other("KDE keyboard service owner changed").into());
            }
        }
        if header
            .sender()
            .is_some_and(|value| value.as_str() == owner.as_str())
            && header
                .interface()
                .is_some_and(|value| value.as_str() == INTERFACE)
        {
            // Requery instead of replaying queued indices: signals received during
            // `query` must never overwrite the snapshot with an older layout.
            tracker
                .observe(|| query(&proxy, read_config))
                .map_err(zbus::Error::from)?;
        }
    }
    Err(io::Error::other("layout signal stream closed").into())
}

pub fn current_layout() -> io::Result<Arc<Layout>> {
    let connection = Connection::session().map_err(io::Error::other)?;
    let proxy = Proxy::new(&connection, SERVICE, PATH, INTERFACE).map_err(io::Error::other)?;
    query(&proxy, &read_configuration)
}

pub fn target_layout(name: &str) -> io::Result<Arc<Layout>> {
    crate::xkb::validate_layout_identifier(name)?;
    let connection = Connection::session().map_err(io::Error::other)?;
    let proxy = Proxy::new(&connection, SERVICE, PATH, INTERFACE).map_err(io::Error::other)?;
    let names: Vec<(String, String, String)> = proxy
        .call("getLayoutsList", &())
        .map_err(io::Error::other)?;
    configured_target(&names, &read_configuration()?, name)
}

fn configured_target(
    names: &[(String, String, String)],
    contents: &str,
    name: &str,
) -> io::Result<Arc<Layout>> {
    let index = names
        .iter()
        .position(|entry| entry.0 == name)
        .ok_or_else(|| {
            io::Error::other(format!("Layout {name:?} is absent from KDE configuration"))
        })?;
    configured_layout(contents, index, name)
        .map(Arc::new)
        .ok_or_else(|| io::Error::other(format!("Cannot match KDE layout {name:?} to kxkbrc")))
}

fn layout_index(name: &str) -> io::Result<usize> {
    let connection = Connection::session().map_err(io::Error::other)?;
    let proxy = Proxy::new(&connection, SERVICE, PATH, INTERFACE).map_err(io::Error::other)?;
    let names: Vec<(String, String, String)> = proxy
        .call("getLayoutsList", &())
        .map_err(io::Error::other)?;
    names
        .iter()
        .position(|entry| entry.0 == name)
        .ok_or_else(|| {
            io::Error::other(format!("Layout {name:?} is absent from KDE configuration"))
        })
}

fn read_configuration() -> io::Result<String> {
    fs::read_to_string(config_path()?)
}

fn query(
    proxy: &Proxy<'_>,
    read_config: &impl Fn() -> io::Result<String>,
) -> io::Result<Arc<Layout>> {
    let names: Vec<(String, String, String)> = proxy
        .call("getLayoutsList", &())
        .map_err(io::Error::other)?;
    let contents = read_config()?;
    let index: u32 = proxy.call("getLayout", &()).map_err(io::Error::other)?;
    let name = &names
        .get(index as usize)
        .ok_or_else(|| io::Error::other("unknown KDE layout index"))?
        .0;
    configured_layout(&contents, index as usize, name)
        .map(Arc::new)
        .ok_or_else(|| io::Error::other("cannot match KDE layout to kxkbrc"))
}

fn config_path() -> io::Result<PathBuf> {
    let directory = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    directory
        .map(|path| path.join(CONFIG_FILE))
        .ok_or_else(|| io::Error::other("cannot find KDE configuration"))
}

fn configured_layout(contents: &str, index: usize, name: &str) -> Option<Layout> {
    let mut is_layout_group = false;
    let mut values = std::collections::HashMap::new();
    for line in contents.lines().map(str::trim) {
        if line.starts_with('[') {
            is_layout_group = line == "[Layout]";
            continue;
        }
        if !is_layout_group || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            values.insert(key.trim(), value.trim());
        }
    }
    let layout = values.get("LayoutList")?.split(',').nth(index)?;
    if layout != name || layout.is_empty() {
        return None;
    }
    Some(Layout {
        name: layout.into(),
        variant: values
            .get("VariantList")
            .and_then(|value| value.split(',').nth(index))
            .unwrap_or_default()
            .into(),
        model: values.get("Model").copied().unwrap_or("pc104").into(),
        options: values.get("Options").copied().unwrap_or_default().into(),
    })
}

const QDBUS_NAMES: [&str; 3] = ["qdbus-qt6", "qdbus6", "qdbus"];
const EXECUTABLE_BITS: u32 = 0o111;

/// KDE integration stays independent of input devices and word history.
pub struct KdePlasma {
    qdbus: PathBuf,
}

impl KdePlasma {
    pub fn new() -> io::Result<Self> {
        let path = env::var_os("PATH").unwrap_or_default();
        let qdbus = find_qdbus(&path)?;
        log::debug!("qdbus executable: {}", qdbus.display());
        Ok(Self { qdbus })
    }

    /// Let KDE select the next layout in its configured order, including wraparound.
    pub fn switch_to_next_layout(&self) -> io::Result<()> {
        self.call_layout_method("org.kde.KeyboardLayouts.switchToNextLayout", &[])
    }

    pub fn switch_to_layout(&self, name: &str) -> io::Result<()> {
        let index = layout_index(name)?.to_string();
        self.call_layout_method("org.kde.KeyboardLayouts.setLayout", &[&index])
    }

    fn call_layout_method(&self, method: &str, arguments: &[&str]) -> io::Result<()> {
        let output = Command::new(&self.qdbus)
            .args(["org.kde.keyboard", "/Layouts", method])
            .args(arguments)
            .output()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot run {}: {error}", self.qdbus.display()),
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let message = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Err(io::Error::other(format!(
                "KDE layout switch via {} failed ({}): {message}",
                self.qdbus.display(),
                output.status
            )));
        }

        if String::from_utf8_lossy(&output.stdout).trim() == "false" {
            return Err(io::Error::other("KDE rejected the layout switch"));
        }

        Ok(())
    }
}

fn find_qdbus(path: &OsStr) -> io::Result<PathBuf> {
    for name in QDBUS_NAMES {
        for directory in env::split_paths(path) {
            let candidate = directory.join(name);
            if let Ok(metadata) = candidate.metadata()
                && metadata.is_file()
                && metadata.permissions().mode() & EXECUTABLE_BITS != 0
            {
                return std::path::absolute(candidate);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "no qdbus executable found; tried {} in PATH={}",
            QDBUS_NAMES.join(", "),
            path.to_string_lossy()
        ),
    ))
}

#[cfg(test)]
mod tests;
