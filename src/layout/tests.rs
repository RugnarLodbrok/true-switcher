use super::*;
use crate::history::Stroke;
use evdev::KeyCode;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

const CONFIG: &str = "[Layout]\nLayoutList=us,ru\nVariantList=,phonetic\nModel=pc105\nOptions=\n";
const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const TEST_POLL: Duration = Duration::from_millis(5);

#[test]
#[ignore = "requires a running KDE session and its kxkbrc"]
fn reads_running_kde_layout() {
    let layout = current_layout().expect("read layout from the running KDE session");
    assert!(!layout.name.is_empty());
    assert!(layout.text(&strokes()).is_some());
}

fn strokes() -> Vec<Stroke> {
    [KeyCode::KEY_G, KeyCode::KEY_H, KeyCode::KEY_B]
        .into_iter()
        .enumerate()
        .map(|(index, key)| Stroke {
            key,
            shift: index == 0,
        })
        .collect()
}

#[test]
fn converts_shift_and_layout_without_changing_physical_keys() {
    let us = Layout {
        name: "us".into(),
        ..Layout::default()
    };
    let ru = Layout {
        name: "ru".into(),
        ..Layout::default()
    };
    assert_eq!(us.text(&strokes()).as_deref(), Some("Ghb"));
    assert_eq!(ru.text(&strokes()).as_deref(), Some("При"));
    assert_eq!(
        us.text(&[Stroke {
            key: KeyCode::KEY_COMMA,
            shift: true
        }])
        .as_deref(),
        Some("<")
    );
}

#[test]
fn rejects_unconvertible_keys_and_invalid_layout() {
    let us = Layout {
        name: "us".into(),
        ..Layout::default()
    };
    assert_eq!(
        us.text(&[Stroke {
            key: KeyCode::KEY_ENTER,
            shift: false
        }]),
        None
    );
    let invalid = Layout {
        name: "\0".into(),
        ..us
    };
    assert_eq!(invalid.text(&strokes()), None);
}

#[test]
fn configuration_preserves_variant_model_and_options_and_rejects_mismatch() {
    let layout = configured_layout(CONFIG, 1, "ru").unwrap();
    assert_eq!(layout.name, "ru");
    assert_eq!(layout.variant, "phonetic");
    assert_eq!(layout.model, "pc105");
    assert_eq!(layout.text(&strokes()).as_deref(), Some("Гхб"));
    assert!(configured_layout(CONFIG, 0, "ru").is_none());
    assert!(configured_layout(CONFIG, 2, "us").is_none());
    let contents = "[Other]\nLayoutList=ru\n[Layout]\nLayoutList=us\nOptions=shift:both_capslock\n";
    assert_eq!(
        configured_layout(contents, 0, "us").unwrap().options,
        "shift:both_capslock"
    );
}

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
            .expect("start isolated test D-Bus");
        let mut address = String::new();
        BufReader::new(process.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        assert!(
            !address.trim().is_empty(),
            "test D-Bus did not provide an address"
        );
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

    fn service(&self, is_racing: bool) -> (Connection, Arc<AtomicU32>) {
        let index = Arc::new(AtomicU32::new(0));
        let mock = KeyboardLayouts {
            index: index.clone(),
            is_racing: AtomicBool::new(is_racing),
        };
        let connection = zbus::blocking::connection::Builder::address(self.address.as_str())
            .unwrap()
            .serve_at(PATH, mock)
            .unwrap()
            .name(SERVICE)
            .unwrap()
            .build()
            .unwrap();
        (connection, index)
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

struct KeyboardLayouts {
    index: Arc<AtomicU32>,
    is_racing: AtomicBool,
}

#[zbus::interface(name = "org.kde.KeyboardLayouts")]
impl KeyboardLayouts {
    #[zbus(name = "getLayout")]
    async fn get_layout(&self, #[zbus(connection)] connection: &zbus::Connection) -> u32 {
        let index = self.index.load(Ordering::SeqCst);
        if self.is_racing.swap(false, Ordering::SeqCst) {
            self.index.store(1, Ordering::SeqCst);
            connection
                .emit_signal(None::<&str>, PATH, INTERFACE, "layoutChanged", &(1_u32,))
                .await
                .unwrap();
        }
        index
    }

    #[zbus(name = "getLayoutsList")]
    fn get_layouts_list(&self) -> Vec<(String, String, String)> {
        vec![
            ("us".into(), String::new(), "English (US)".into()),
            ("ru".into(), String::new(), "Russian".into()),
        ]
    }
}

fn wait_for(tracker: &LayoutTracker, name: &str) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while tracker.current().is_none_or(|layout| layout.name != name) {
        assert!(Instant::now() < deadline, "tracker did not reach {name}");
        thread::sleep(TEST_POLL);
    }
}

fn watch(bus: &TestBus, tracker: &LayoutTracker) -> thread::JoinHandle<zbus::Result<()>> {
    let connection = bus.connect();
    let tracker = tracker.clone();
    thread::spawn(move || track_connection(&connection, &tracker, &|| Ok(CONFIG.into())))
}

#[test]
fn subscribes_before_initial_query_and_handles_changes_during_query() {
    let bus = TestBus::start();
    let (service, _) = bus.service(true);
    let tracker = LayoutTracker::default();
    let worker = watch(&bus, &tracker);
    wait_for(&tracker, "ru");
    service.release_name(SERVICE).unwrap();
    assert!(worker.join().unwrap().is_err());
    assert!(tracker.current().is_none());
}

#[test]
fn gets_initial_layout_receives_changes_and_resubscribes_after_owner_loss() {
    let bus = TestBus::start();
    let (service, index) = bus.service(false);
    let tracker = LayoutTracker::default();
    let worker = watch(&bus, &tracker);
    wait_for(&tracker, "us");
    index.store(1, Ordering::SeqCst);
    service
        .emit_signal(None::<&str>, PATH, INTERFACE, "layoutChanged", &(1_u32,))
        .unwrap();
    wait_for(&tracker, "ru");
    service.release_name(SERVICE).unwrap();
    assert!(worker.join().unwrap().is_err());
    assert!(tracker.current().is_none());
    let (replacement, _) = bus.service(false);
    let worker = watch(&bus, &tracker);
    wait_for(&tracker, "us");
    replacement.release_name(SERVICE).unwrap();
    assert!(worker.join().unwrap().is_err());
}
