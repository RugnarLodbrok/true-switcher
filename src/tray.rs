mod menu;

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{Configuration, ConfigurationFile};
use crate::input_events::{InputEventMessage, InputEventSender, InputEvents};
use crate::lifecycle::ShutdownReason;
use zbus::blocking::{Connection, Proxy, fdo::DBusProxy};
use zbus::zvariant::{ObjectPath, OwnedValue};

const APP_ID: &str = "true-switcher";
const APP_TITLE: &str = "True Switcher";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const ICON_NAME: &str = "icon";
const ERROR_ICON_NAME: &str = "icon_error";
const INSTALLED_ICONS: &str = "/usr/local/share/true-switcher/icons";
const SOURCE_ICONS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/icon");
const RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const DEFAULT_NOTIFICATION_TIMEOUT: i32 = -1;

type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

pub struct Tray;

impl Tray {
    pub fn start(
        sender: InputEventSender,
        config: &Configuration,
        config_file: ConfigurationFile,
    ) -> io::Result<Self> {
        let runtime = menu::RuntimeMenu::new(
            sender.clone(),
            config,
            config_file,
            crate::kde::layout_count,
        );
        let mut menu = menu::Menu::new(sender);
        menu.runtime = Some(Arc::new(runtime));
        thread::Builder::new()
            .name("system-tray".into())
            .spawn(move || maintain(menu))?;
        Ok(Self)
    }

    pub fn run_error(
        error: String,
        sender: InputEventSender,
        events: InputEvents,
    ) -> io::Result<ShutdownReason> {
        let connection = Connection::session().map_err(io::Error::other)?;
        export(&connection, menu::Menu::new(sender), Some(error.clone()))
            .map_err(io::Error::other)?;
        let mut owner = None;
        register(&connection, &mut owner)
            .map_err(|error| io::Error::other(format!("tray registration: {error}")))?;
        notify(&connection, "Configuration error", &error)
            .map_err(|error| io::Error::other(format!("notification: {error}")))?;
        loop {
            if let Some(reason) = wait_for_shutdown(&events, Instant::now() + RETRY_DELAY)? {
                return Ok(reason);
            }
            register(&connection, &mut owner)
                .map_err(|error| io::Error::other(format!("tray registration: {error}")))?;
            DBusProxy::new(&connection)
                .and_then(|proxy| {
                    proxy
                        .get_name_owner(NOTIFICATIONS.try_into()?)
                        .map(|_| ())
                        .map_err(Into::into)
                })
                .map_err(|error| io::Error::other(format!("notification service: {error}")))?;
        }
    }
}

fn export(connection: &Connection, menu: menu::Menu, error: Option<String>) -> zbus::Result<()> {
    connection.object_server().at(ITEM_PATH, Item { error })?;
    connection.object_server().at(MENU_PATH, menu)?;
    Ok(())
}

fn wait_for_shutdown(
    events: &InputEvents,
    deadline: Instant,
) -> io::Result<Option<ShutdownReason>> {
    while let Some(event) = events.wait(Some(deadline))? {
        match event {
            InputEventMessage::Shutdown(reason) => return Ok(Some(reason)),
            InputEventMessage::DeviceError(error) => return Err(error),
            _ => {}
        }
    }
    Ok(None)
}

fn maintain(menu: menu::Menu) {
    let mut delay = RETRY_DELAY;
    loop {
        match maintain_connection(&mut delay, &menu) {
            Ok(()) => delay = RETRY_DELAY,
            Err(error) => {
                log::warn!("true-switcher: tray registration: {error}");
                thread::sleep(delay);
                delay = (delay * 2).min(MAX_RETRY_DELAY);
            }
        }
    }
}

fn maintain_connection(delay: &mut Duration, menu: &menu::Menu) -> zbus::Result<()> {
    let connection = Connection::session()?;
    menu.refresh();
    export(&connection, menu.clone(), None)?;
    let mut owner = None;
    loop {
        register(&connection, &mut owner)?;
        *delay = RETRY_DELAY;
        thread::sleep(RETRY_DELAY);
    }
}

fn register(connection: &Connection, registered_owner: &mut Option<String>) -> zbus::Result<()> {
    let owner = match DBusProxy::new(connection)?.get_name_owner(WATCHER.try_into()?) {
        Ok(owner) => owner.to_string(),
        Err(error) => {
            *registered_owner = None;
            return Err(error.into());
        }
    };
    if registered_owner.as_ref() == Some(&owner) {
        return Ok(());
    }
    let proxy = Proxy::new(connection, owner.as_str(), WATCHER_PATH, WATCHER)?;
    proxy.call::<_, _, ()>("RegisterStatusNotifierItem", &(ITEM_PATH,))?;
    drop(proxy);
    *registered_owner = Some(owner);
    Ok(())
}

fn icon_directory() -> &'static str {
    if Path::new(INSTALLED_ICONS)
        .join(format!("{ICON_NAME}.svg"))
        .is_file()
    {
        INSTALLED_ICONS
    } else {
        SOURCE_ICONS
    }
}

fn notify(connection: &Connection, summary: &str, body: &str) -> zbus::Result<()> {
    let proxy = Proxy::new(connection, NOTIFICATIONS, NOTIFICATIONS_PATH, NOTIFICATIONS)?;
    let icon = format!("{}/{ICON_NAME}.svg", icon_directory());
    let hints: HashMap<&str, OwnedValue> = HashMap::new();
    let _: u32 = proxy.call(
        "Notify",
        &(
            APP_TITLE,
            0_u32,
            icon,
            summary,
            body,
            Vec::<String>::new(),
            hints,
            DEFAULT_NOTIFICATION_TIMEOUT,
        ),
    )?;
    Ok(())
}

struct Item {
    error: Option<String>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        APP_ID
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        APP_TITLE
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> &str {
        icon_directory()
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        if self.error.is_some() {
            ERROR_ICON_NAME
        } else {
            ICON_NAME
        }
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }

    #[zbus(property)]
    fn attention_movie_name(&self) -> &str {
        ""
    }

    #[zbus(property, name = "ToolTip")]
    fn tool_tip(&self) -> (&str, Pixmaps, &str, &str) {
        (
            self.icon_name(),
            Vec::new(),
            APP_TITLE,
            self.error
                .as_deref()
                .unwrap_or("Reload to apply configuration changes"),
        )
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'_> {
        ObjectPath::try_from(MENU_PATH).unwrap()
    }

    fn activate(&self, x: i32, y: i32) {
        let _ = (x, y);
    }
    fn secondary_activate(&self, x: i32, y: i32) {
        let _ = (x, y);
    }
    fn context_menu(&self, x: i32, y: i32) {
        let _ = (x, y);
    }
    fn scroll(&self, delta: i32, orientation: &str) {
        let _ = (delta, orientation);
    }
}

#[cfg(test)]
mod tests;
