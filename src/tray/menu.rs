use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::thread;

use crate::input_events::InputEventSender;
use crate::lifecycle::ShutdownReason;
use zbus::zvariant::{OwnedValue, Str, Value};

const ROOT_ID: i32 = 0;
const RELOAD_ID: i32 = 1;
const QUIT_ID: i32 = 2;
const REVISION: u32 = 1;
const PROTOCOL_VERSION: u32 = 3;
const CLICKED: &str = "clicked";

type Properties = HashMap<String, OwnedValue>;
pub(super) type Layout = (i32, Properties, Vec<OwnedValue>);
type Event = (i32, String, OwnedValue, u32);
type Restart = Arc<dyn Fn() -> io::Result<()> + Send + Sync>;

pub(super) struct Menu {
    pub(super) restart: Restart,
    pub(super) stop: Restart,
}

impl Menu {
    pub(super) fn new(sender: InputEventSender) -> Self {
        let restart = sender.clone();
        Self {
            restart: Arc::new(move || restart.shutdown(ShutdownReason::Restart)),
            stop: Arc::new(move || sender.shutdown(ShutdownReason::Stop)),
        }
    }

    fn dispatch(
        &self,
        connection: &zbus::Connection,
        id: i32,
        event: &str,
    ) -> zbus::fdo::Result<()> {
        if ![ROOT_ID, RELOAD_ID, QUIT_ID].contains(&id) {
            return Err(zbus::fdo::Error::InvalidArgs("Unknown menu item".into()));
        }
        if [RELOAD_ID, QUIT_ID].contains(&id) && event == CLICKED {
            let (action, verb, title) = if id == RELOAD_ID {
                (self.restart.clone(), "restart", "Reload failed")
            } else {
                (self.stop.clone(), "stop", "Quit failed")
            };
            let connection = connection.clone();
            thread::Builder::new()
                .name(format!("tray-{verb}"))
                .spawn(move || {
                    if let Err(error) = action() {
                        let body = format!("Cannot {verb} True Switcher: {error}");
                        log::warn!("true-switcher: {body}");
                        let _ = super::notify(&connection.into(), title, &body);
                    }
                })
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        }
        Ok(())
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
        QUIT_ID => HashMap::from([
            ("label".into(), Str::from("Quit").into()),
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
            for id in [RELOAD_ID, QUIT_ID] {
                let child: Layout = (id, properties(id, &property_names)?, Vec::new());
                children.push(Value::from(child).try_into().map_err(
                    |error: zbus::zvariant::Error| zbus::fdo::Error::Failed(error.to_string()),
                )?);
            }
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
            vec![ROOT_ID, RELOAD_ID, QUIT_ID]
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
                .filter(|id| ![ROOT_ID, RELOAD_ID, QUIT_ID].contains(id))
                .collect(),
        )
    }
}
