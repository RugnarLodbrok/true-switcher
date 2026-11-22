use super::*;
use crate::config::RuntimeSettings;
use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use zbus::zvariant::Value;

const ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
const MENU_INTERFACE: &str = "com.canonical.dbusmenu";
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

struct TestBus {
    process: Child,
    address: String,
}

impl TestBus {
    fn start() -> Self {
        let mut process = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(process.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        assert!(!address.trim().is_empty());
        Self {
            process,
            address: address.trim().into(),
        }
    }

    fn connect(&self) -> Connection {
        zbus::blocking::connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .unwrap()
    }

    fn watcher(&self) -> (Connection, Arc<Mutex<Vec<String>>>) {
        let registrations = Arc::new(Mutex::new(Vec::new()));
        let connection = self.connect();
        connection
            .object_server()
            .at(WATCHER_PATH, Watcher(registrations.clone()))
            .unwrap();
        connection.request_name(WATCHER).unwrap();
        (connection, registrations)
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

struct Watcher(Arc<Mutex<Vec<String>>>);

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: &str) {
        self.0.lock().unwrap().push(service.into());
    }
}

#[test]
fn registers_when_watcher_appears_and_after_it_is_replaced() {
    let bus = TestBus::start();
    let tray = bus.connect();
    let (sender, _events) = InputEvents::new();
    export(&tray, menu::Menu::new(sender), None).unwrap();
    let mut owner = None;
    assert!(register(&tray, &mut owner).is_err());
    let (watcher, registrations) = bus.watcher();
    register(&tray, &mut owner).unwrap();
    register(&tray, &mut owner).unwrap();
    assert_eq!(*registrations.lock().unwrap(), [ITEM_PATH]);
    watcher.release_name(WATCHER).unwrap();
    let (_replacement, registrations) = bus.watcher();
    register(&tray, &mut owner).unwrap();
    assert_eq!(*registrations.lock().unwrap(), [ITEM_PATH]);
}

#[test]
fn exports_icon_and_reload_menu_and_dispatches_only_clicks() {
    let bus = TestBus::start();
    let tray = bus.connect();
    let (sent, received) = mpsc::channel();
    let (quit_sent, quit_received) = mpsc::channel();
    export(
        &tray,
        menu::Menu {
            restart: Arc::new(move || {
                sent.send(()).unwrap();
                Ok(())
            }),
            stop: Arc::new(move || {
                quit_sent.send(()).unwrap();
                Ok(())
            }),
            runtime: None,
        },
        None,
    )
    .unwrap();
    let client = bus.connect();
    let destination = tray.unique_name().unwrap().as_str();
    let item = Proxy::new(&client, destination, ITEM_PATH, ITEM_INTERFACE).unwrap();
    assert_eq!(item.get_property::<String>("IconName").unwrap(), ICON_NAME);
    assert!(item.get_property::<bool>("ItemIsMenu").unwrap());
    assert_eq!(
        item.get_property::<ObjectPath<'_>>("Menu")
            .unwrap()
            .as_str(),
        MENU_PATH
    );
    let tooltip: (String, Pixmaps, String, String) = item.get_property("ToolTip").unwrap();
    assert_eq!(tooltip.2, APP_TITLE);
    let menu = Proxy::new(&client, destination, MENU_PATH, MENU_INTERFACE).unwrap();
    let (_, layout): (u32, menu::Layout) = menu
        .call("GetLayout", &(0_i32, -1_i32, Vec::<String>::new()))
        .unwrap();
    assert_eq!(layout.2.len(), 2);
    let mut children = layout.2.into_iter();
    let child = menu::Layout::try_from(children.next().unwrap()).unwrap();
    assert_eq!(child.0, 1);
    assert_eq!(
        <&str>::try_from(child.1.get("label").unwrap()).unwrap(),
        "Reload"
    );
    let child = menu::Layout::try_from(children.next().unwrap()).unwrap();
    assert_eq!(child.0, 2);
    assert_eq!(
        <&str>::try_from(child.1.get("label").unwrap()).unwrap(),
        "Quit"
    );
    let (_, shallow): (u32, menu::Layout) = menu
        .call("GetLayout", &(0_i32, 0_i32, Vec::<String>::new()))
        .unwrap();
    assert!(shallow.2.is_empty());
    menu.call::<_, _, ()>("Event", &(1_i32, "hovered", Value::from(0_i32), 0_u32))
        .unwrap();
    assert!(received.try_recv().is_err());
    menu.call::<_, _, ()>("Event", &(1_i32, "clicked", Value::from(0_i32), 0_u32))
        .unwrap();
    received.recv_timeout(TEST_TIMEOUT).unwrap();
    menu.call::<_, _, ()>("Event", &(2_i32, "hovered", Value::from(0_i32), 0_u32))
        .unwrap();
    assert!(quit_received.try_recv().is_err());
    menu.call::<_, _, ()>("Event", &(2_i32, "clicked", Value::from(0_i32), 0_u32))
        .unwrap();
    quit_received.recv_timeout(TEST_TIMEOUT).unwrap();
    assert!(
        menu.call::<_, _, ()>("Event", &(99_i32, "clicked", Value::from(0_i32), 0_u32))
            .is_err()
    );
    assert!(received.try_recv().is_err());
}

#[test]
fn error_item_exposes_error_icon_and_details() {
    let bus = TestBus::start();
    let tray = bus.connect();
    let error = "Invalid switch_key";
    let (sender, _events) = InputEvents::new();
    export(&tray, menu::Menu::new(sender), Some(error.into())).unwrap();
    let client = bus.connect();
    let item = Proxy::new(
        &client,
        tray.unique_name().unwrap().as_str(),
        ITEM_PATH,
        ITEM_INTERFACE,
    )
    .unwrap();
    assert_eq!(
        item.get_property::<String>("IconName").unwrap(),
        ERROR_ICON_NAME
    );
    let tooltip: (String, Pixmaps, String, String) = item.get_property("ToolTip").unwrap();
    assert_eq!(tooltip.0, ERROR_ICON_NAME);
    assert_eq!(tooltip.3, error);
    assert_eq!(
        item.get_property::<ObjectPath<'_>>("Menu")
            .unwrap()
            .as_str(),
        MENU_PATH
    );
}

struct Notifications(mpsc::Sender<(String, String)>);

#[test]
fn menu_requests_shutdown_and_error_mode_receives_the_same_reasons() {
    let (sender, events) = InputEvents::new();
    let menu = menu::Menu::new(sender);
    (menu.restart)().unwrap();
    assert_eq!(
        wait_for_shutdown(&events, Instant::now() + TEST_TIMEOUT).unwrap(),
        Some(ShutdownReason::Restart)
    );
    (menu.stop)().unwrap();
    assert_eq!(
        wait_for_shutdown(&events, Instant::now() + TEST_TIMEOUT).unwrap(),
        Some(ShutdownReason::Stop)
    );
}

fn click(menu: &Proxy<'_>, id: i32) {
    menu.call::<_, _, ()>("Event", &(id, "clicked", Value::from(0_i32), 0_u32))
        .unwrap();
}

fn menu_property<T: TryFrom<OwnedValue>>(menu: &Proxy<'_>, id: i32, name: &str) -> T
where
    T::Error: std::fmt::Debug,
{
    let value: OwnedValue = menu.call("GetProperty", &(id, name)).unwrap();
    T::try_from(value).unwrap()
}

#[test]
fn runtime_menu_exports_log_levels_in_a_submenu() {
    let bus = TestBus::start();
    let tray = bus.connect();
    let client = bus.connect();
    let (sender, _events) = InputEvents::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, format!("{RUNTIME_CONFIG}log_level = 'debug'\n")).unwrap();
    export_runtime_menu(&tray, sender, &path, || Ok(2));
    let menu = Proxy::new(
        &client,
        tray.unique_name().unwrap().as_str(),
        MENU_PATH,
        MENU_INTERFACE,
    )
    .unwrap();

    let find_submenu = |depth: i32| {
        let (_, layout): (u32, menu::Layout) = menu
            .call("GetLayout", &(0_i32, depth, Vec::<String>::new()))
            .unwrap();
        layout
            .2
            .into_iter()
            .map(|child| menu::Layout::try_from(child).unwrap())
            .find(|(_, properties, _)| {
                <&str>::try_from(&properties["label"]).unwrap() == "Log level"
            })
            .unwrap()
    };
    let (submenu_id, properties, children) = find_submenu(1);
    assert_eq!(
        <&str>::try_from(&properties["children-display"]).unwrap(),
        "submenu"
    );
    assert!(children.is_empty());
    assert_eq!(find_submenu(-1).2.len(), 6);

    let (_, submenu): (u32, menu::Layout) = menu
        .call("GetLayout", &(submenu_id, 1_i32, Vec::<String>::new()))
        .unwrap();
    let levels: Vec<_> = submenu
        .2
        .into_iter()
        .map(|child| {
            let (_, mut properties, children) = menu::Layout::try_from(child).unwrap();
            assert!(children.is_empty());
            assert_eq!(
                <&str>::try_from(&properties["toggle-type"]).unwrap(),
                "radio"
            );
            (
                String::try_from(properties.remove("label").unwrap()).unwrap(),
                i32::try_from(&properties["toggle-state"]).unwrap(),
            )
        })
        .collect();
    assert_eq!(
        levels
            .iter()
            .map(|(name, state)| (name.as_str(), *state))
            .collect::<Vec<_>>(),
        [
            ("off", 0),
            ("error", 0),
            ("warn", 0),
            ("info", 0),
            ("debug", 1),
            ("trace", 0)
        ]
    );
}

const RUNTIME_CONFIG: &str = "switch_key = 'KEY_PAUSE'\nauto_switch_sound = false\n";

fn export_runtime_menu(
    tray: &Connection,
    sender: InputEventSender,
    path: &Path,
    layout_count: impl Fn() -> io::Result<usize> + Send + Sync + 'static,
) -> RuntimeSettings {
    let (config, file) = crate::config::load_from(path).unwrap();
    let settings = RuntimeSettings::from(&config);
    let runtime = menu::RuntimeMenu::new(sender.clone(), &config, file, layout_count);
    let mut exported_menu = menu::Menu::new(sender);
    exported_menu.runtime = Some(Arc::new(runtime));
    exported_menu.refresh();
    export(tray, exported_menu, None).unwrap();
    settings
}

fn start_input(
    events: InputEvents,
    mut settings: RuntimeSettings,
) -> thread::JoinHandle<ShutdownReason> {
    thread::spawn(move || {
        loop {
            match events.wait(Some(Instant::now() + TEST_TIMEOUT)).unwrap() {
                Some(InputEventMessage::ToggleSetting(setting, reply)) => {
                    settings.toggle(setting);
                    reply.send(settings).unwrap();
                }
                Some(InputEventMessage::Shutdown(reason)) => return reason,
                _ => panic!("Expected a setting change or shutdown"),
            }
        }
    })
}

#[test]
fn runtime_menu_applies_toggles_and_checks_layout_count_again_on_click() {
    let bus = TestBus::start();
    let tray = bus.connect();
    let client = bus.connect();
    let (sender, events) = InputEvents::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, RUNTIME_CONFIG).unwrap();
    let count = Arc::new(AtomicUsize::new(2));
    let settings = export_runtime_menu(&tray, sender, &path, {
        let count = count.clone();
        move || Ok(count.load(Ordering::Relaxed))
    });
    let input = start_input(events, settings);
    let menu = Proxy::new(
        &client,
        tray.unique_name().unwrap().as_str(),
        MENU_PATH,
        MENU_INTERFACE,
    )
    .unwrap();
    assert!(menu_property::<bool>(&menu, 3, "enabled"));
    let mut updates = menu.receive_signal("ItemsPropertiesUpdated").unwrap();
    count.store(3, Ordering::Relaxed);
    click(&menu, 3);
    let update = updates.next().unwrap();
    type MenuUpdates = (
        Vec<(i32, HashMap<String, OwnedValue>)>,
        Vec<(i32, Vec<String>)>,
    );
    let (updated, removed): MenuUpdates = update.body().deserialize().unwrap();
    assert_eq!(updated[0].0, 3);
    assert!(!bool::try_from(&updated[0].1["enabled"]).unwrap());
    assert!(removed.is_empty());
    assert!(!menu_property::<bool>(&menu, 3, "enabled"));
    assert_eq!(menu_property::<i32>(&menu, 3, "toggle-state"), 0);
    assert!(
        menu_property::<String>(&menu, 3, "label").contains("(requires at most two KDE layouts)")
    );

    count.store(2, Ordering::Relaxed);
    assert!(menu.call::<_, _, bool>("AboutToShow", &(0_i32,)).unwrap());
    click(&menu, 3);
    assert_eq!(menu_property::<i32>(&menu, 3, "toggle-state"), 1);
    for id in [4, 5, 6] {
        assert_eq!(
            menu_property::<String>(&menu, id, "toggle-type"),
            "checkmark"
        );
        assert_eq!(menu_property::<i32>(&menu, id, "toggle-state"), 0);
        click(&menu, id);
        assert_eq!(menu_property::<i32>(&menu, id, "toggle-state"), 1);
    }

    count.store(3, Ordering::Relaxed);
    assert!(!menu.call::<_, _, bool>("AboutToShow", &(0_i32,)).unwrap());
    assert!(menu_property::<bool>(&menu, 3, "enabled"));
    assert_eq!(menu_property::<i32>(&menu, 3, "toggle-state"), 1);
    click(&menu, 3);
    assert!(!menu_property::<bool>(&menu, 3, "enabled"));
    assert_eq!(menu_property::<i32>(&menu, 3, "toggle-state"), 0);
    click(&menu, 2);
    assert_eq!(input.join().unwrap(), ShutdownReason::Stop);
}

#[test]
fn runtime_menu_changes_disk_only_on_save_and_reload_reads_saved_settings() {
    let bus = TestBus::start();
    let client = bus.connect();
    let server = bus.connect();
    let (notifications, received) = mpsc::channel();
    server
        .object_server()
        .at(NOTIFICATIONS_PATH, Notifications(notifications))
        .unwrap();
    server.request_name(NOTIFICATIONS).unwrap();

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, RUNTIME_CONFIG).unwrap();
    for (is_selected, command) in [(false, 1), (false, 7), (true, 2)] {
        let (sender, events) = InputEvents::new();
        let tray = bus.connect();
        let settings = export_runtime_menu(&tray, sender, &path, || Ok(2));
        let input = start_input(events, settings);
        let menu = Proxy::new(
            &client,
            tray.unique_name().unwrap().as_str(),
            MENU_PATH,
            MENU_INTERFACE,
        )
        .unwrap();
        assert_eq!(
            menu_property::<i32>(&menu, 4, "toggle-state"),
            i32::from(is_selected)
        );
        if !is_selected {
            click(&menu, 4);
            assert_eq!(fs::read_to_string(&path).unwrap(), RUNTIME_CONFIG);
        }

        click(&menu, command);
        if command == 7 {
            let saved = fs::read_to_string(&path).unwrap();
            assert!(saved.contains("pattern_switch = true"));
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            click(&menu, 7);
            assert!(path.is_dir());
            assert_eq!(menu_property::<i32>(&menu, 4, "toggle-state"), 1);
            fs::remove_dir(&path).unwrap();
            fs::write(&path, saved).unwrap();
            click(&menu, 1);
        }
        let expected = if command == 2 {
            ShutdownReason::Stop
        } else {
            ShutdownReason::Restart
        };
        assert_eq!(input.join().unwrap(), expected);
    }
    assert!(received.try_recv().is_err());
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        assert_eq!(app_name, APP_TITLE);
        assert_eq!(replaces_id, 0);
        assert!(app_icon.ends_with(".svg"));
        assert!(actions.is_empty());
        assert!(hints.is_empty());
        assert_eq!(expire_timeout, DEFAULT_NOTIFICATION_TIMEOUT);
        self.0.send((summary.into(), body.into())).unwrap();
        1
    }
}

#[test]
fn sends_warning_and_reports_failed_reload_as_notification() {
    let bus = TestBus::start();
    let server = bus.connect();
    let (sent, received) = mpsc::channel();
    server
        .object_server()
        .at(NOTIFICATIONS_PATH, Notifications(sent))
        .unwrap();
    server.request_name(NOTIFICATIONS).unwrap();
    let tray = bus.connect();
    notify(&tray, "Configuration warning", "Using KEY_PAUSE").unwrap();
    assert_eq!(
        received.recv_timeout(TEST_TIMEOUT).unwrap(),
        ("Configuration warning".into(), "Using KEY_PAUSE".into())
    );
    export(
        &tray,
        menu::Menu {
            restart: Arc::new(|| Err(io::Error::other("Permission denied"))),
            stop: Arc::new(|| Ok(())),
            runtime: None,
        },
        None,
    )
    .unwrap();
    let client = bus.connect();
    let menu = Proxy::new(
        &client,
        tray.unique_name().unwrap().as_str(),
        MENU_PATH,
        MENU_INTERFACE,
    )
    .unwrap();
    menu.call::<_, _, ()>("Event", &(1_i32, "clicked", Value::from(0_i32), 0_u32))
        .unwrap();
    let (summary, body) = received.recv_timeout(TEST_TIMEOUT).unwrap();
    assert_eq!(summary, "Reload failed");
    assert!(body.contains("Permission denied"));
}
