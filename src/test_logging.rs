struct TestLogger;

thread_local! {
    static MESSAGES: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

impl log::Log for TestLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        MESSAGES.with(|messages| messages.borrow().is_some())
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        MESSAGES.with(|messages| {
            if let Some(messages) = messages.borrow_mut().as_mut() {
                messages.push(message);
            }
        });
    }

    fn flush(&self) {}
}

pub(crate) fn capture<T>(run: impl FnOnce() -> T) -> (T, Vec<String>) {
    static LOGGER: TestLogger = TestLogger;
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
    });
    MESSAGES.with(|messages| messages.replace(Some(Vec::new())));
    let result = run();
    let messages = MESSAGES.with(|messages| messages.take().unwrap());
    (result, messages)
}
