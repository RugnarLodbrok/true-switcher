mod config;
mod environment;
mod focus;
mod history;
mod hold;
mod input;
mod input_events;
mod layout;
pub mod output;
mod sound;
mod statistics;
mod switch_rules;
mod tray;

use std::io;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

const KEYBOARD_DISCOVERY_DELAY: Duration = Duration::from_millis(500);

struct Correction {
    previous: Option<history::Word>,
    word: Option<history::Word>,
    target: Option<Arc<layout::Layout>>,
    mode: statistics::SwitchMode,
    is_sound_enabled: bool,
}

impl Correction {
    fn from_input(
        input: &input::Input,
        is_auto_sound: bool,
        is_manual_sound: bool,
    ) -> Option<Self> {
        let is_auto = input.is_auto_request();
        let target = if is_auto {
            Some(input.automatic_target()?)
        } else {
            None
        };
        Some(Self {
            previous: input.previous_word().cloned(),
            word: input.latest_word().cloned(),
            target,
            mode: if is_auto {
                statistics::SwitchMode::Auto
            } else {
                statistics::SwitchMode::Manual
            },
            is_sound_enabled: if is_auto {
                is_auto_sound
            } else {
                is_manual_sound
            },
        })
    }
}

struct Switcher {
    desktop: environment::KdePlasma,
    keyboard: output::KeyboardOutput,
    statistics: statistics::Statistics,
    sound: sound::Sound,
    layouts: layout::LayoutTracker,
}

fn run() -> io::Result<()> {
    let (config, auto_switch) = match prepare_configuration() {
        Ok(prepared) => prepared,
        Err(error) => {
            log::error!("true-switcher: {error}");
            return tray::Tray::run_error(error);
        }
    };

    log::set_max_level(config.log_level);
    let _tray = tray::Tray::start()?;

    let desktop = environment::KdePlasma::new()?;
    let keyboard = output::KeyboardOutput::new()?;
    let statistics = statistics::Statistics::start();
    let sound = sound::Sound::start(config.manual_switch_sound || config.auto_switch_sound);

    let (sender, events) = input_events::InputEvents::new();
    let _focus_service = focus::FocusService::start(sender.clone())?;
    let layouts = layout::LayoutTracker::start(sender.clone());

    let history = history::History::default()
        .with_statistics(statistics.clone())
        .with_auto_switch(auto_switch);
    let mut input = input::Input::open(events, sender, config.switch_key, history)?;

    let mut switcher = Switcher {
        desktop,
        keyboard,
        statistics,
        sound,
        layouts,
    };

    input.wait_for_keyboard_discovery(KEYBOARD_DISCOVERY_DELAY)?;
    log::info!("Ready");

    switcher.process_requests(&mut input, &config)
}

fn prepare_configuration() -> Result<(config::Configuration, switch_rules::AutoSwitchRules), String>
{
    config::load().and_then(|config| {
        switch_rules::AutoSwitchRules::new(&config)
            .map(|auto_switch| (config, auto_switch))
            .map_err(|error| format!("Configuration: {error}"))
    })
}

impl Switcher {
    fn process_requests(
        &mut self,
        input: &mut input::Input,
        config: &config::Configuration,
    ) -> io::Result<()> {
        loop {
            input.wait_for_switch_request()?;

            if !input.enter()? {
                continue;
            }

            if let Some(correction) =
                Correction::from_input(input, config.auto_switch_sound, config.manual_switch_sound)
            {
                self.correct(input, &correction)?;
            }

            input.leave()?;
        }
    }

    fn correct(&mut self, input: &mut input::Input, correction: &Correction) -> io::Result<()> {
        let is_cancelled = AtomicBool::new(input.read_captured()?);
        thread::scope(|scope| -> io::Result<()> {
            let worker = scope.spawn(|| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    correct_word(
                        correction,
                        &mut self.keyboard,
                        &self.desktop,
                        &self.statistics,
                        &self.sound,
                        &is_cancelled,
                    )
                }))
                .unwrap_or_else(|_| Err(io::Error::other("correction thread panicked")));
                self.layouts.finish_correction(result);
            });
            let mut read_error = None;
            let result = loop {
                match input.wait_for_correction() {
                    Ok(result) => {
                        if input.take_cancel_request() {
                            is_cancelled.store(true, Ordering::Relaxed);
                        }
                        if let Some(result) = result {
                            break result;
                        }
                    }
                    Err(error) => {
                        is_cancelled.store(true, Ordering::Relaxed);
                        read_error = Some(error);
                        break Ok(());
                    }
                }
            };
            worker
                .join()
                .map_err(|_| io::Error::other("correction thread panicked"))?;
            if let Some(error) = read_error {
                return Err(error);
            }
            if is_cancelled.load(Ordering::Relaxed) {
                input.reset_history();
            }
            result
        })
    }
}

fn correct_word(
    correction: &Correction,
    keyboard: &mut output::KeyboardOutput,
    desktop: &environment::KdePlasma,
    statistics: &statistics::Statistics,
    sound: &sound::Sound,
    is_cancelled: &AtomicBool,
) -> io::Result<()> {
    if is_cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    log::debug!("SYNTHETIC erase");
    if let Some(word) = &correction.word {
        keyboard.erase(word.entries.len() + word.trailing_spaces)?;
    }
    if is_cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    switch_and_replay(
        || match &correction.target {
            Some(target) => desktop.switch_to_layout(&target.name),
            None => desktop.switch_to_next_layout(),
        },
        || {
            if correction.is_sound_enabled {
                sound.play();
            }
        },
        || {
            record_switch(
                statistics,
                correction.previous.as_ref(),
                correction.word.as_ref(),
                correction.mode,
            )
        },
        || {
            log::debug!("SYNTHETIC replay");
            if let Some(word) = &correction.word {
                keyboard.replay(word)?;
            }
            Ok(())
        },
        is_cancelled,
    )
    .map(|_| ())
}

fn record_switch(
    statistics: &statistics::Statistics,
    previous: Option<&history::Word>,
    word: Option<&history::Word>,
    mode: statistics::SwitchMode,
) -> Option<Arc<layout::Layout>> {
    let time = SystemTime::now();
    match layout::current_layout() {
        Ok(target) => {
            statistics.record_switch_with_mode(previous, word, &target, time, mode);
            Some(target)
        }
        Err(error) => {
            log::warn!("true-switcher: skipping statistics: {error}");
            None
        }
    }
}

fn switch_and_replay<T>(
    switch: impl FnOnce() -> io::Result<()>,
    play_sound: impl FnOnce(),
    record: impl FnOnce() -> T,
    replay: impl FnOnce() -> io::Result<()>,
    is_cancelled: &AtomicBool,
) -> io::Result<T> {
    switch()?;
    play_sound();
    let result = record();
    if !is_cancelled.load(Ordering::Relaxed) {
        replay()?;
    }
    Ok(result)
}

fn main() -> ExitCode {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Trace)
        .init();
    log::set_max_level(log::LevelFilter::Info);
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("true-switcher: {error}");
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
            || calls.borrow_mut().push("sound"),
            || calls.borrow_mut().push("record"),
            || {
                calls.borrow_mut().push("replay");
                Ok(())
            },
            &is_cancelled,
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["switch", "sound", "record"]);
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
            || calls.borrow_mut().push("sound"),
            || calls.borrow_mut().push("record"),
            || {
                calls.borrow_mut().push("replay");
                Err(io::Error::other("replay failed"))
            },
            &is_cancelled,
        );
        assert!(result.is_err());
        assert_eq!(*calls.borrow(), ["switch", "sound", "record", "replay"]);
        calls.borrow_mut().clear();
        let result = switch_and_replay(
            || Err(io::Error::other("switch failed")),
            || calls.borrow_mut().push("sound"),
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
