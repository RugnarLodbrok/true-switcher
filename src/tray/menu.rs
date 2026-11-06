use std::collections::HashMap;
use std::io;
use std::process::Command;
use std::sync::Arc;
use std::thread;

use zbus::zvariant::{OwnedValue, Str, Value};

const ROOT_ID: i32 = 0;
const RELOAD_ID: i32 = 1;
const REVISION: u32 = 1;
const PROTOCOL_VERSION: u32 = 3;
const SERVICE: &str = "true-switcher.service";
const CLICKED: &str = "clicked";

type Properties = HashMap<String, OwnedValue>;
pub(super) type Layout = (i32, Properties, Vec<OwnedValue>);
type Event = (i32, String, OwnedValue, u32);
type Restart = Arc<dyn Fn() -> io::Result<()> + Send + Sync>;

pub(super) struct Menu {
    pub(super) restart: Restart,
}

impl Menu {
    pub(super) fn new() -> Self {
        Self {
            restart: Arc::new(restart_service),
        }
    }

    fn dispatch(
        &self,
        connection: &zbus::Connection,
        id: i32,
        event: &str,
    ) -> zbus::fdo::Result<()> {
        if ![ROOT_ID, RELOAD_ID].contains(&id) {
            return Err(zbus::fdo::Error::InvalidArgs("Unknown menu item".into()));
        }
        if id == RELOAD_ID && event == CLICKED {
            let restart = self.restart.clone();
            let connection = connection.clone();
            thread::Builder::new()
                .name("tray-reload".into())
                .spawn(move || {
                    if let Err(error) = restart() {
                        let body = format!("Cannot restart {SERVICE}: {error}");
                        eprintln!("true-switcher: {body}");
                        let _ = super::notify(&connection.into(), "Reload failed", &body);
                    }
                })
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        }
        Ok(())
    }
}

fn restart_service() -> io::Result<()> {
    let output = Command::new("systemctl")
        .args(["--system", "--no-block", "restart", SERVICE])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn properties(id: i32, names: &[String]) -> zbus::fdo::Result<Properties> {
    let mut properties = match id {
        ROOT_ID => HashMap::from([("children-display".into(), Str::from("submenu").into())]),
        RELOAD_ID => HashMap::from([
            ("label".into(), Str::from("Reload").into()),
            ("enabled".into(), true.into()),
            ("visible".into(), true.into()),
        ]),
        _ => return Err(zbus::fdo::Error::InvalidArgs("Unknown menu item".into())),
    };
    if !names.is_empty() {
        properties.retain(|name, _| names.contains(name));
    }
    Ok(properties)
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        PROTOCOL_VERSION
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<&str> {
        vec![super::icon_directory()]
    }

    #[zbus(out_args("revision", "layout"))]
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let mut children = Vec::new();
        if parent_id == ROOT_ID && recursion_depth != 0 {
            let child: Layout = (
                RELOAD_ID,
                properties(RELOAD_ID, &property_names)?,
                Vec::new(),
            );
            children.push(Value::from(child).try_into().map_err(
                |error: zbus::zvariant::Error| zbus::fdo::Error::Failed(error.to_string()),
            )?);
        }
        Ok((
            REVISION,
            (parent_id, properties(parent_id, &property_names)?, children),
        ))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, Properties)> {
        let ids = if ids.is_empty() {
            vec![ROOT_ID, RELOAD_ID]
        } else {
            ids
        };
        ids.into_iter()
            .filter_map(|id| {
                properties(id, &property_names)
                    .ok()
                    .map(|props| (id, props))
            })
            .collect()
    }

    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<OwnedValue> {
        properties(id, &[])?
            .remove(name)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("Unknown property".into()))
    }

    fn event(
        &self,
        id: i32,
        event_id: &str,
        data: OwnedValue,
        timestamp: u32,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let _ = (data, timestamp);
        self.dispatch(connection, id, event_id)
    }

    fn event_group(
        &self,
        events: Vec<Event>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> Vec<i32> {
        events
            .into_iter()
            .filter_map(|(id, event, _, _)| self.dispatch(connection, id, &event).err().map(|_| id))
            .collect()
    }

    fn about_to_show(&self, id: i32) -> zbus::fdo::Result<bool> {
        properties(id, &[])?;
        Ok(false)
    }

    #[zbus(out_args("updates_needed", "id_errors"))]
    fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (
            Vec::new(),
            ids.into_iter()
                .filter(|id| ![ROOT_ID, RELOAD_ID].contains(id))
                .collect(),
        )
    }
}
