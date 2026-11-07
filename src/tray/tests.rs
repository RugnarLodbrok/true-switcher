use super::*;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
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
    export(&tray, menu::Menu::new(), None).unwrap();
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
    export(&tray, menu::Menu::new(), Some(error.into())).unwrap();
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
