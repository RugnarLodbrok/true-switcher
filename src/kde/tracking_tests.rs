use super::*;
use crate::word::Stroke;
use evdev::KeyCode;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

const CONFIG: &str = "[Layout]\nLayoutList=us,ru\nVariantList=,phonetic\nModel=pc105\nOptions=\n";
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn uses_first_duplicate_layout_with_its_configured_variant() {
    let names = vec![
        ("ru".into(), String::new(), String::new()),
        ("ru".into(), String::new(), String::new()),
    ];
    let contents = "[Layout]\nLayoutList=ru,ru\nVariantList=phonetic,\n";
    assert_eq!(
        configured_target(&names, contents, "ru").unwrap().variant,
        "phonetic"
    );
    assert!(configured_target(&names, contents, "us").is_err());
}

#[test]
#[ignore = "requires a running KDE session and its kxkbrc"]
fn reads_running_kde_layout() {
    let layout = current_layout().expect("read layout from the running KDE session");
    assert!(!layout.name.is_empty());
    assert!(layout.to_text(&strokes()).is_ok());
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
fn configuration_preserves_variant_model_and_options_and_rejects_mismatch() {
    let layout = configured_layout(CONFIG, 1, "ru").unwrap();
    assert_eq!(layout.name, "ru");
    assert_eq!(layout.variant, "phonetic");
    assert_eq!(layout.model, "pc105");
    assert_eq!(layout.to_text(&strokes()).as_deref(), Ok("Гхб"));
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

fn wait_for(events: &crate::input_events::InputEvents, name: Option<&str>) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        match events
            .wait(Some(deadline))
            .unwrap()
            .expect("layout event before deadline")
        {
            InputEventMessage::LayoutChanged(layout)
                if layout.as_ref().map(|value| value.name.as_str()) == name =>
            {
                return;
            }
            _ => {}
        }
    }
}

fn watch(bus: &TestBus, tracker: &LayoutTracker) -> thread::JoinHandle<zbus::Result<()>> {
    let connection = bus.connect();
    let tracker = tracker.clone();
    thread::spawn(move || {
        track_connection(&connection, &tracker, &|| Ok(CONFIG.into()), &mut || {})
    })
}

#[test]
fn serializes_layout_queries_and_completion_publication() {
    use std::sync::mpsc;
    let (sender, events) = crate::input_events::InputEvents::new();
    let tracker = LayoutTracker::new(sender);
    let (started, started_query) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let observer = tracker.clone();
    let worker = thread::spawn(move || {
        observer.observe(|| {
            started.send(()).unwrap();
            released.recv_timeout(TEST_TIMEOUT).unwrap();
            Ok(Arc::new(Layout {
                name: "us".into(),
                ..Default::default()
            }))
        })
    });
    started_query.recv_timeout(TEST_TIMEOUT).unwrap();
    let completion = tracker.clone();
    let (queried, completion_query) = mpsc::channel();
    let finished = thread::spawn(move || {
        completion.finish_with(Ok(()), || {
            queried.send(()).unwrap();
            Ok(Arc::new(Layout {
                name: "ru".into(),
                ..Default::default()
            }))
        })
    });
    assert!(completion_query.try_recv().is_err());
    assert!(events.available().unwrap().is_none());
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    finished.join().unwrap();
    wait_for(&events, Some("us"));
    match events.available().unwrap().unwrap() {
        InputEventMessage::CorrectionFinished { result, layout } => {
            assert!(result.is_ok());
            assert_eq!(layout.unwrap().name, "ru");
        }
        _ => panic!("expected correction completion after older observation"),
    }
    tracker
        .observe(|| {
            Ok(Arc::new(Layout {
                name: "us".into(),
                ..Default::default()
            }))
        })
        .unwrap();
    wait_for(&events, Some("us"));
}

#[test]
fn subscribes_before_initial_query_and_handles_changes_during_query() {
    let bus = TestBus::start();
    let (service, _) = bus.service(true);
    let (sender, events) = crate::input_events::InputEvents::new();
    let tracker = LayoutTracker::new(sender);
    let worker = watch(&bus, &tracker);
    wait_for(&events, Some("ru"));
    service.release_name(SERVICE).unwrap();
    assert!(worker.join().unwrap().is_err());
    wait_for(&events, None);
}

#[test]
fn gets_initial_layout_receives_changes_and_resubscribes_after_owner_loss() {
    let bus = TestBus::start();
    let (service, index) = bus.service(false);
    let (sender, events) = crate::input_events::InputEvents::new();
    let tracker = LayoutTracker::new(sender);
    let worker = watch(&bus, &tracker);
    wait_for(&events, Some("us"));
    index.store(1, Ordering::SeqCst);
    service
        .emit_signal(None::<&str>, PATH, INTERFACE, "layoutChanged", &(1_u32,))
        .unwrap();
    wait_for(&events, Some("ru"));
    service.release_name(SERVICE).unwrap();
    assert!(worker.join().unwrap().is_err());
    wait_for(&events, None);
    let (replacement, _) = bus.service(false);
    let worker = watch(&bus, &tracker);
    wait_for(&events, Some("us"));
    replacement.release_name(SERVICE).unwrap();
    assert!(worker.join().unwrap().is_err());
}
