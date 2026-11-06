mod config;
mod environment;
mod focus;
mod history;
mod input;
mod layout;
pub mod output;
mod statistics;
mod tray;

use std::io;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

fn run() -> io::Result<()> {
    let config = config::load();
    if let Some(warning) = &config.warning {
        eprintln!("true-switcher: {warning}");
    }
    let _tray = tray::Tray::start(config.warning)
        .map_err(|error| {
            eprintln!("true-switcher: cannot start tray: {error}");
        })
        .ok();
    let desktop = environment::KdePlasma::new()?;
    let mut keyboard = output::KeyboardOutput::new()?;
    let focus_service = focus::FocusService::start()?;
    let layouts = layout::LayoutTracker::start();
    let statistics = statistics::Statistics::start();
    let mut input = input::Input::open(focus_service.changes(), config.switch_key, layouts)?;

    // Give the desktop time to discover the uinput keyboard before accepting input.
    thread::sleep(Duration::from_millis(500));
    eprintln!("Ready");

    loop {
        input.wait_for_request()?;
        input.enter()?;
        let is_cancelled = AtomicBool::new(input.read_captured()?);
        let word = input.latest_word().cloned();
        let previous = input.previous_word().cloned();
        thread::scope(|scope| -> io::Result<()> {
            let worker = scope.spawn(|| {
                correct_word(
                    previous.as_ref(),
                    word.as_ref(),
                    &mut keyboard,
                    &desktop,
                    &statistics,
                    &is_cancelled,
                )
            });
            let mut read_error = None;
            while !worker.is_finished() {
                match input.read_captured() {
                    Ok(true) => is_cancelled.store(true, Ordering::Relaxed),
                    Ok(false) => {}
                    Err(error) => {
                        is_cancelled.store(true, Ordering::Relaxed);
                        read_error = Some(error);
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
            if input.read_captured()? {
                is_cancelled.store(true, Ordering::Relaxed);
            }
            let result = worker
                .join()
                .map_err(|_| io::Error::other("correction thread panicked"))?;
            if let Some(error) = read_error {
                return Err(error);
            }
            if is_cancelled.load(Ordering::Relaxed) {
                input.reset_history();
            } else if let Ok(layout) = &result {
                input.set_latest_layout(layout.clone());
            }
            result.map(|_| ())
        })?;
        input.leave()?;
    }
}

fn correct_word(
    previous: Option<&history::Word>,
    word: Option<&history::Word>,
    keyboard: &mut output::KeyboardOutput,
    desktop: &environment::KdePlasma,
    statistics: &statistics::Statistics,
    is_cancelled: &AtomicBool,
) -> io::Result<Option<Arc<layout::Layout>>> {
    if is_cancelled.load(Ordering::Relaxed) {
        return Ok(None);
    }
    eprintln!("SYNTHETIC erase");
    if let Some(word) = word {
        keyboard.erase(word.strokes.len() + word.trailing_spaces)?;
    }
    if is_cancelled.load(Ordering::Relaxed) {
        return Ok(None);
    }
    switch_and_replay(
        || desktop.switch_to_next_layout(),
        || record_switch(statistics, previous, word),
        || {
            eprintln!("SYNTHETIC replay");
            if let Some(word) = word {
                keyboard.replay(word)?;
            }
            Ok(())
        },
        is_cancelled,
    )
}

fn record_switch(
    statistics: &statistics::Statistics,
    previous: Option<&history::Word>,
    word: Option<&history::Word>,
) -> Option<Arc<layout::Layout>> {
    let time = SystemTime::now();
    match layout::current_layout() {
        Ok(target) => {
            statistics.record_switch(previous, word, &target, time);
            Some(target)
        }
        Err(error) => {
            eprintln!("true-switcher: skipping statistics: {error}");
            None
        }
    }
}

fn switch_and_replay<T>(
    switch: impl FnOnce() -> io::Result<()>,
    record: impl FnOnce() -> T,
    replay: impl FnOnce() -> io::Result<()>,
    is_cancelled: &AtomicBool,
) -> io::Result<T> {
    switch()?;
    let result = record();
    if !is_cancelled.load(Ordering::Relaxed) {
        replay()?;
    }
    Ok(result)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("true-switcher: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn records_successful_switch_even_when_cancelled_before_replay() {
        let is_cancelled = AtomicBool::new(false);
        let calls = RefCell::new(Vec::new());
        switch_and_replay(
            || {
                calls.borrow_mut().push("switch");
                is_cancelled.store(true, Ordering::Relaxed);
                Ok(())
            },
            || calls.borrow_mut().push("record"),
            || {
                calls.borrow_mut().push("replay");
                Ok(())
            },
            &is_cancelled,
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["switch", "record"]);
    }

    #[test]
    fn records_before_replay_error_and_skips_record_when_switch_fails() {
        let is_cancelled = AtomicBool::new(false);
        let calls = RefCell::new(Vec::new());
        let result = switch_and_replay(
            || {
                calls.borrow_mut().push("switch");
                Ok(())
            },
            || calls.borrow_mut().push("record"),
            || {
                calls.borrow_mut().push("replay");
                Err(io::Error::other("replay failed"))
            },
            &is_cancelled,
        );
        assert!(result.is_err());
        assert_eq!(*calls.borrow(), ["switch", "record", "replay"]);
        calls.borrow_mut().clear();
        let result = switch_and_replay(
            || Err(io::Error::other("switch failed")),
            || calls.borrow_mut().push("record"),
            || {
                calls.borrow_mut().push("replay");
                Ok(())
            },
            &is_cancelled,
        );
        assert!(result.is_err());
        assert!(calls.borrow().is_empty());
    }
}
