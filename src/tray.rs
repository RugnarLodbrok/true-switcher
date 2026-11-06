mod menu;

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::thread;
use std::time::Duration;

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
const INSTALLED_ICONS: &str = "/usr/local/share/true-switcher/icons";
const SOURCE_ICONS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/icon");
const RETRY_DELAY: Duration = Duration::from_secs(1);
const DEFAULT_NOTIFICATION_TIMEOUT: i32 = -1;

type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

pub struct Tray {
    _connection: Connection,
}

impl Tray {
    pub fn start(warning: Option<String>) -> io::Result<Self> {
        let connection = Connection::session().map_err(io::Error::other)?;
        export(&connection, menu::Menu::new()).map_err(io::Error::other)?;
        let worker = connection.clone();
        thread::Builder::new()
            .name("system-tray".into())
            .spawn(move || {
                maintain(&worker, warning);
            })?;
        Ok(Self {
            _connection: connection,
        })
    }
}

fn export(connection: &Connection, menu: menu::Menu) -> zbus::Result<()> {
    connection.object_server().at(ITEM_PATH, Item)?;
    connection.object_server().at(MENU_PATH, menu)?;
    Ok(())
}

fn maintain(connection: &Connection, mut warning: Option<String>) {
    let mut owner = None;
    let mut is_error_reported = false;
    loop {
        match register(connection, &mut owner) {
            Ok(()) => is_error_reported = false,
            Err(error) => {
                if !is_error_reported {
                    eprintln!("true-switcher: tray registration: {error}");
                }
                is_error_reported = true;
            }
        }
        if let Some(body) = warning.as_deref()
            && notify(connection, "Configuration warning", body).is_ok()
        {
            warning = None;
        }
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

struct Item;

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
        ICON_NAME
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
            ICON_NAME,
            Vec::new(),
            APP_TITLE,
            "Reload to apply configuration changes",
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
