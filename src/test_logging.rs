struct TestLogger;

struct Capture {
    level: log::LevelFilter,
    messages: Vec<String>,
}

thread_local! {
    static CAPTURE: std::cell::RefCell<Option<Capture>> = const { std::cell::RefCell::new(None) };
}

impl log::Log for TestLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        CAPTURE.with(|capture| {
            capture
                .borrow()
                .as_ref()
                .is_some_and(|capture| metadata.level() <= capture.level)
        })
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        CAPTURE.with(|capture| {
            if let Some(capture) = capture.borrow_mut().as_mut() {
                capture.messages.push(message);
            }
        });
    }

    fn flush(&self) {}
}

pub(crate) fn capture<T>(run: impl FnOnce() -> T) -> (T, Vec<String>) {
    capture_at(log::LevelFilter::Trace, run)
}

pub(crate) fn capture_at<T>(level: log::LevelFilter, run: impl FnOnce() -> T) -> (T, Vec<String>) {
    static LOGGER: TestLogger = TestLogger;
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
    });
    CAPTURE.with(|capture| {
        capture.replace(Some(Capture {
            level,
            messages: Vec::new(),
        }));
    });
    let result = run();
    let messages = CAPTURE.with(|capture| capture.take().unwrap().messages);
    (result, messages)
}
