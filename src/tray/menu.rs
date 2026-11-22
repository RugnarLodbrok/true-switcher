use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use crate::config::{
    Configuration, ConfigurationFile, LOG_LEVELS, RuntimeSetting, RuntimeSettings,
};
use crate::dictionary::DICTIONARY_COUNT;
use crate::input_events::{InputEventMessage, InputEventSender};
use crate::lifecycle::ShutdownReason;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Str, Value};

const ROOT_ID: i32 = 0;
const RELOAD_ID: i32 = 1;
const QUIT_ID: i32 = 2;
const AUTO_SWITCH_ID: i32 = 3;
const PATTERN_SWITCH_ID: i32 = 4;
const MANUAL_SOUND_ID: i32 = 5;
const AUTO_SOUND_ID: i32 = 6;
const SAVE_ID: i32 = 7;
const LOG_LEVEL_MENU_ID: i32 = 8;
const LOG_LEVEL_IDS: [i32; LOG_LEVELS.len()] = [9, 10, 11, 12, 13, 14];
const SETTING_IDS: [i32; 4] = [
    AUTO_SWITCH_ID,
    PATTERN_SWITCH_ID,
    MANUAL_SOUND_ID,
    AUTO_SOUND_ID,
];
const REVISION: u32 = 1;
const PROTOCOL_VERSION: u32 = 3;
const CLICKED: &str = "clicked";

type Properties = HashMap<String, OwnedValue>;
pub(super) type Layout = (i32, Properties, Vec<OwnedValue>);
type Event = (i32, String, OwnedValue, u32);
type LifecycleAction = Arc<dyn Fn() -> io::Result<()> + Send + Sync>;

#[derive(Clone)]
pub(super) struct Menu {
    pub(super) restart: LifecycleAction,
    pub(super) stop: LifecycleAction,
    pub(super) runtime: Option<Arc<RuntimeMenu>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AutoSwitchAvailability {
    Available,
    TooManyLayouts,
    UnknownLayoutCount,
}

struct MenuState {
    settings: RuntimeSettings,
    availability: AutoSwitchAvailability,
    log_level: log::LevelFilter,
}

pub(super) struct RuntimeMenu {
    sender: InputEventSender,
    state: Mutex<MenuState>,
    config_file: ConfigurationFile,
    layout_count: Box<dyn Fn() -> io::Result<usize> + Send + Sync>,
}

impl RuntimeMenu {
    pub(super) fn new(
        sender: InputEventSender,
        config: &Configuration,
        config_file: ConfigurationFile,
        layout_count: impl Fn() -> io::Result<usize> + Send + Sync + 'static,
    ) -> Self {
        Self {
            sender,
            state: Mutex::new(MenuState {
                settings: config.into(),
                availability: AutoSwitchAvailability::UnknownLayoutCount,
                log_level: config.log_level,
            }),
            config_file,
            layout_count: Box::new(layout_count),
        }
    }

    fn refresh_availability(&self, state: &mut MenuState) -> bool {
        let availability = if state.settings.is_auto_switch {
            AutoSwitchAvailability::Available
        } else {
            match (self.layout_count)() {
                Ok(..=DICTIONARY_COUNT) => AutoSwitchAvailability::Available,
                Ok(_) => AutoSwitchAvailability::TooManyLayouts,
                Err(error) => {
                    log::warn!("true-switcher: cannot check KDE layouts: {error}");
                    AutoSwitchAvailability::UnknownLayoutCount
                }
            }
        };
        let is_changed = state.availability != availability;
        state.availability = availability;
        is_changed
    }

    fn toggle(&self, setting: RuntimeSetting) -> zbus::fdo::Result<()> {
        let mut state = self.state.lock().unwrap();
        if matches!(setting, RuntimeSetting::AutoSwitch) && !state.settings.is_auto_switch {
            self.refresh_availability(&mut state);
            if state.availability != AutoSwitchAvailability::Available {
                return Ok(());
            }
        }
        let (reply, result) = mpsc::channel();
        self.sender
            .send(InputEventMessage::ToggleSetting(setting, reply));
        state.settings = result
            .recv()
            .map_err(|_| zbus::fdo::Error::Failed("Input event channel closed".into()))?;
        if matches!(setting, RuntimeSetting::AutoSwitch) {
            self.refresh_availability(&mut state);
        }
        Ok(())
    }

    fn save(&self) {
        let state = self.state.lock().unwrap();
        if let Err(error) = self.config_file.save(state.settings, state.log_level) {
            log::warn!("true-switcher: cannot save configuration: {error}");
        }
    }

    fn select_log_level(&self, id: i32, level: log::LevelFilter) -> Vec<(i32, Properties)> {
        let mut state = self.state.lock().unwrap();
        if state.log_level == level {
            return Vec::new();
        }
        let previous_id = LOG_LEVEL_IDS
            .into_iter()
            .zip(LOG_LEVELS)
            .find_map(|(id, (_, level))| (level == state.log_level).then_some(id))
            .expect("all log levels have menu items");
        state.log_level = level;
        log::set_max_level(level);
        vec![(previous_id, toggle_state(false)), (id, toggle_state(true))]
    }

    fn log_level_properties(&self, label: &str, level: log::LevelFilter) -> Properties {
        let mut properties = item_properties(label, true);
        properties.insert("toggle-type".into(), Str::from("radio").into());
        let is_checked = self.state.lock().unwrap().log_level == level;
        properties.insert("toggle-state".into(), i32::from(is_checked).into());
        properties
    }

    fn properties(&self, setting: RuntimeSetting) -> Properties {
        let state = self.state.lock().unwrap();
        let (label, is_checked) = match setting {
            RuntimeSetting::AutoSwitch => (
                match state.availability {
                    AutoSwitchAvailability::Available => "Auto-switch",
                    AutoSwitchAvailability::TooManyLayouts => {
                        "Auto-switch (requires at most two KDE layouts)"
                    }
                    AutoSwitchAvailability::UnknownLayoutCount => {
                        "Auto-switch (cannot read KDE layouts)"
                    }
                },
                state.settings.is_auto_switch,
            ),
            RuntimeSetting::PatternSwitch => ("Pattern switch", state.settings.is_pattern_switch),
            RuntimeSetting::ManualSwitchSound => {
                ("Manual switch sound", state.settings.is_manual_switch_sound)
            }
            RuntimeSetting::AutoSwitchSound => {
                ("Auto-switch sound", state.settings.is_auto_switch_sound)
            }
        };
        let is_enabled = !matches!(setting, RuntimeSetting::AutoSwitch)
            || state.settings.is_auto_switch
            || state.availability == AutoSwitchAvailability::Available;
        let mut properties = item_properties(label, is_enabled);
        properties.insert("toggle-type".into(), Str::from("checkmark").into());
        properties.insert("toggle-state".into(), i32::from(is_checked).into());
        properties
    }
}

fn setting(id: i32) -> Option<RuntimeSetting> {
    match id {
        AUTO_SWITCH_ID => Some(RuntimeSetting::AutoSwitch),
        PATTERN_SWITCH_ID => Some(RuntimeSetting::PatternSwitch),
        MANUAL_SOUND_ID => Some(RuntimeSetting::ManualSwitchSound),
        AUTO_SOUND_ID => Some(RuntimeSetting::AutoSwitchSound),
        _ => None,
    }
}

fn log_level(id: i32) -> Option<(&'static str, log::LevelFilter)> {
    LOG_LEVEL_IDS
        .into_iter()
        .zip(LOG_LEVELS)
        .find_map(|(item_id, level)| (item_id == id).then_some(level))
}

fn item_properties(label: &str, is_enabled: bool) -> Properties {
    HashMap::from([
        ("label".into(), Str::from(label).into()),
        ("enabled".into(), is_enabled.into()),
        ("visible".into(), true.into()),
    ])
}

fn toggle_state(is_checked: bool) -> Properties {
    HashMap::from([("toggle-state".into(), i32::from(is_checked).into())])
}

impl Menu {
    pub(super) fn new(sender: InputEventSender) -> Self {
        let restart = sender.clone();
        Self {
            restart: Arc::new(move || restart.shutdown(ShutdownReason::Restart)),
            stop: Arc::new(move || sender.shutdown(ShutdownReason::Stop)),
            runtime: None,
        }
    }

    pub(super) fn refresh(&self) -> bool {
        self.runtime
            .as_ref()
            .is_some_and(|runtime| runtime.refresh_availability(&mut runtime.state.lock().unwrap()))
    }

    fn root_children(&self) -> Vec<i32> {
        let mut ids = Vec::new();
        if self.runtime.is_some() {
            ids.extend(SETTING_IDS);
            ids.extend([LOG_LEVEL_MENU_ID, SAVE_ID]);
        }
        ids.extend([RELOAD_ID, QUIT_ID]);
        ids
    }

    fn ids(&self) -> Vec<i32> {
        let mut ids = self.root_children();
        if self.runtime.is_some() {
            ids.extend(LOG_LEVEL_IDS);
        }
        ids
    }

    fn layout(&self, id: i32, depth: i32, names: &[String]) -> zbus::fdo::Result<Layout> {
        let properties = self.properties(id, names)?;
        let child_ids = match (id, depth) {
            (_, 0) => Vec::new(),
            (ROOT_ID, _) => self.root_children(),
            (LOG_LEVEL_MENU_ID, _) => LOG_LEVEL_IDS.to_vec(),
            _ => Vec::new(),
        };
        let child_depth = if depth < 0 { depth } else { depth - 1 };
        let mut children = Vec::with_capacity(child_ids.len());
        for child_id in child_ids {
            let child = self.layout(child_id, child_depth, names)?;
            children.push(Value::from(child).try_into().map_err(
                |error: zbus::zvariant::Error| zbus::fdo::Error::Failed(error.to_string()),
            )?);
        }
        Ok((id, properties, children))
    }

    async fn dispatch(
        &self,
        emitter: &SignalEmitter<'_>,
        id: i32,
        event: &str,
    ) -> zbus::fdo::Result<()> {
        if id != ROOT_ID && !self.ids().contains(&id) {
            return Err(zbus::fdo::Error::InvalidArgs("Unknown menu item".into()));
        }
        if event != CLICKED || id == ROOT_ID {
            return Ok(());
        }
        if let Some(runtime) = &self.runtime {
            if let Some((_, level)) = log_level(id) {
                let updates = runtime.select_log_level(id, level);
                if updates.is_empty() {
                    return Ok(());
                }
                return Self::items_properties_updated(emitter, updates, Vec::new())
                    .await
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()));
            }
            if id == LOG_LEVEL_MENU_ID {
                return Ok(());
            }
            if let Some(setting) = setting(id) {
                runtime.toggle(setting)?;
                return Self::items_properties_updated(
                    emitter,
                    vec![(id, runtime.properties(setting))],
                    Vec::new(),
                )
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()));
            }
            if id == SAVE_ID {
                runtime.save();
                return Ok(());
            }
        }
        self.dispatch_lifecycle(emitter.connection(), id)
    }

    fn dispatch_lifecycle(&self, connection: &zbus::Connection, id: i32) -> zbus::fdo::Result<()> {
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
        Ok(())
    }

    fn properties(&self, id: i32, names: &[String]) -> zbus::fdo::Result<Properties> {
        let mut properties = match id {
            ROOT_ID => HashMap::from([("children-display".into(), Str::from("submenu").into())]),
            RELOAD_ID => item_properties("Reload", true),
            QUIT_ID => item_properties("Quit", true),
            SAVE_ID if self.runtime.is_some() => item_properties("Save to config", true),
            LOG_LEVEL_MENU_ID if self.runtime.is_some() => {
                let mut properties = item_properties("Log level", true);
                properties.insert("children-display".into(), Str::from("submenu").into());
                properties
            }
            _ => match (&self.runtime, setting(id), log_level(id)) {
                (Some(runtime), Some(setting), _) => runtime.properties(setting),
                (Some(runtime), _, Some((label, level))) => {
                    runtime.log_level_properties(label, level)
                }
                _ => return Err(zbus::fdo::Error::InvalidArgs("Unknown menu item".into())),
            },
        };
        if !names.is_empty() {
            properties.retain(|name, _| names.contains(name));
        }
        Ok(properties)
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu", spawn = false)]
impl Menu {
    #[zbus(signal)]
    async fn items_properties_updated(
        emitter: &SignalEmitter<'_>,
        updated: Vec<(i32, Properties)>,
        removed: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;

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
        Ok((
            REVISION,
            self.layout(parent_id, recursion_depth, &property_names)?,
        ))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, Properties)> {
        let ids = if ids.is_empty() {
            std::iter::once(ROOT_ID).chain(self.ids()).collect()
        } else {
            ids
        };
        ids.into_iter()
            .filter_map(|id| {
                self.properties(id, &property_names)
                    .ok()
                    .map(|props| (id, props))
            })
            .collect()
    }

    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<OwnedValue> {
        self.properties(id, &[])?
            .remove(name)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("Unknown property".into()))
    }

    async fn event(
        &self,
        id: i32,
        event_id: &str,
        data: OwnedValue,
        timestamp: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let _ = (data, timestamp);
        self.dispatch(&emitter, id, event_id).await
    }

    async fn event_group(
        &self,
        events: Vec<Event>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Vec<i32> {
        let mut errors = Vec::new();
        for (id, event, _, _) in events {
            if self.dispatch(&emitter, id, &event).await.is_err() {
                errors.push(id);
            }
        }
        errors
    }

    fn about_to_show(&self, id: i32) -> zbus::fdo::Result<bool> {
        self.properties(id, &[])?;
        Ok(matches!(id, ROOT_ID | AUTO_SWITCH_ID) && self.refresh())
    }

    #[zbus(out_args("updates_needed", "id_errors"))]
    fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        let (valid, errors): (Vec<_>, Vec<_>) = ids
            .into_iter()
            .partition(|id| *id == ROOT_ID || self.ids().contains(id));
        let updates = valid
            .into_iter()
            .filter(|id| matches!(*id, ROOT_ID | AUTO_SWITCH_ID))
            .collect::<Vec<_>>();
        if !updates.is_empty() && self.refresh() {
            (updates, errors)
        } else {
            (Vec::new(), errors)
        }
    }
}
