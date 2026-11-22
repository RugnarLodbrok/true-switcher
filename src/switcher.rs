use crate::config::RuntimeSettings;
use crate::lifecycle::{ProcessingError, ProcessingResult, ShutdownReason};
use crate::{input, kde, layout, output, sound, statistics, word};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum SwitchMode {
    Manual = 0,
    Pattern = 1,
    Auto = 2,
}

struct Correction {
    previous: Option<word::Word>,
    word: Option<word::Word>,
    target: Option<Arc<layout::Layout>>,
    mode: SwitchMode,
    is_sound_enabled: bool,
}

impl Correction {
    fn from_request(request: input::SwitchRequest, settings: RuntimeSettings) -> Self {
        Self {
            previous: request.previous,
            word: request.word,
            target: request.target,
            mode: request.mode,
            is_sound_enabled: match request.mode {
                SwitchMode::Pattern | SwitchMode::Auto => settings.is_auto_switch_sound,
                SwitchMode::Manual => settings.is_manual_switch_sound,
            },
        }
    }

    fn has_dead_key_in(&self, layout: &layout::Layout) -> bool {
        self.word.as_ref().is_some_and(|word| {
            let space = word::Stroke {
                key: evdev::KeyCode::KEY_SPACE,
                shift: false,
            };
            let strokes = word
                .strokes()
                .chain((word.trailing_spaces > 0).then_some(&space));
            crate::xkb::has_dead_key(layout, strokes) == Ok(true)
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CorrectionOutcome {
    Completed,
    Cancelled,
    Interrupted,
}

pub struct Switcher {
    pub(crate) desktop: kde::KdePlasma,
    pub(crate) keyboard: output::KeyboardOutput,
    pub(crate) statistics: statistics::Statistics,
    pub(crate) sound: sound::Sound,
    pub(crate) layouts: kde::LayoutTracker,
}

impl Switcher {
    pub fn process_requests(&mut self, input: &mut input::Input) -> io::Result<ShutdownReason> {
        loop {
            match input.next_request()? {
                input::InputRequest::Switch => {
                    self.process_request(input)?;
                }
                input::InputRequest::Shutdown(reason) => return Ok(reason),
            }
        }
    }

    fn process_request(&mut self, input: &mut input::Input) -> io::Result<CorrectionOutcome> {
        match self.execute_correction(input) {
            Ok(outcome) => Ok(outcome),
            Err(ProcessingError::Interrupted) => Ok(CorrectionOutcome::Interrupted),
            Err(ProcessingError::Io(error)) => Err(error),
        }
    }

    fn execute_correction(
        &mut self,
        input: &mut input::Input,
    ) -> ProcessingResult<CorrectionOutcome> {
        let Some(mut interception) = input::enter_interceptor(input)? else {
            return Ok(CorrectionOutcome::Cancelled);
        };
        let request = interception
            .take_request()
            .expect("interception accepted a switch request");
        let correction = Correction::from_request(request, interception.input().runtime_settings());

        let outcome = if interception.read_available()? {
            CorrectionOutcome::Cancelled
        } else {
            self.begin_and_correct(interception.as_mut(), &correction)?
        };
        interception.finish()?;
        Ok(outcome)
    }

    fn begin_and_correct(
        &mut self,
        interception: &mut dyn input::CorrectionInterceptor,
        correction: &Correction,
    ) -> ProcessingResult<CorrectionOutcome> {
        let next_index = match &correction.target {
            Some(target) if correction.has_dead_key_in(target) => {
                return Ok(CorrectionOutcome::Cancelled);
            }
            None if correction.word.is_some() => {
                let (index, layout) = kde::next_layout()?;
                if correction.has_dead_key_in(&layout) || interception.read_available()? {
                    return Ok(CorrectionOutcome::Cancelled);
                }
                Some(index)
            }
            _ => None,
        };
        let Self {
            desktop,
            keyboard,
            statistics,
            sound,
            layouts,
        } = self;
        switch_and_correct(
            || match (&correction.target, next_index) {
                (Some(target), _) => desktop.switch_to_layout(target.name()),
                (None, Some(index)) => desktop.switch_to_layout_index(index),
                (None, None) => desktop.switch_to_next_layout(),
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
            |target| {
                interception.input().begin_correction(target);
                Self::correct(keyboard, layouts, interception, correction)
            },
        )
    }

    fn correct(
        keyboard: &mut output::KeyboardOutput,
        layouts: &kde::LayoutTracker,
        interception: &mut dyn input::CorrectionInterceptor,
        correction: &Correction,
    ) -> ProcessingResult<CorrectionOutcome> {
        let is_cancelled = AtomicBool::new(interception.read_available()?);
        thread::scope(|scope| -> ProcessingResult<CorrectionOutcome> {
            let worker = scope.spawn(|| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    correct_word(correction, keyboard, &is_cancelled)
                }))
                .unwrap_or_else(|_| Err(io::Error::other("correction thread panicked")));
                layouts.finish_correction(result);
            });
            let mut read_error = None;
            let result = loop {
                match interception.wait_for_correction() {
                    Ok(result) => {
                        if interception.input().take_cancel_request() {
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
                interception.input().reset_history();
            }
            result?;
            Ok(if is_cancelled.load(Ordering::Relaxed) {
                CorrectionOutcome::Cancelled
            } else {
                CorrectionOutcome::Completed
            })
        })
    }
}

fn correct_word(
    correction: &Correction,
    keyboard: &mut output::KeyboardOutput,
    is_cancelled: &AtomicBool,
) -> io::Result<()> {
    replay_correction(
        keyboard,
        is_cancelled,
        |keyboard| {
            log::debug!("SYNTHETIC erase");
            if let Some(word) = &correction.word {
                keyboard.erase(word.entries.len() + word.trailing_spaces, is_cancelled)?;
            }
            Ok(())
        },
        |keyboard| {
            log::debug!("SYNTHETIC replay");
            if let Some(word) = &correction.word {
                keyboard.replay(word, is_cancelled)?;
            }
            Ok(())
        },
    )
}

pub(crate) fn replay_correction<T>(
    output: &mut T,
    is_cancelled: &AtomicBool,
    erase: impl FnOnce(&mut T) -> io::Result<()>,
    replay: impl FnOnce(&mut T) -> io::Result<()>,
) -> io::Result<()> {
    if is_cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    erase(output)?;
    if is_cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    replay(output)
}

fn record_switch(
    statistics: &statistics::Statistics,
    previous: Option<&word::Word>,
    word: Option<&word::Word>,
    mode: SwitchMode,
) -> Option<Arc<layout::Layout>> {
    let time = SystemTime::now();
    match kde::current_layout() {
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

pub(crate) fn switch_and_correct<T, R, E: From<io::Error>>(
    switch: impl FnOnce() -> io::Result<()>,
    play_sound: impl FnOnce(),
    record: impl FnOnce() -> T,
    correct: impl FnOnce(T) -> Result<R, E>,
) -> Result<R, E> {
    switch()?;
    play_sound();
    let target = record();
    correct(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeSetting;
    use std::cell::RefCell;

    #[test]
    fn correction_uses_each_runtime_sound_setting_independently() {
        let mut settings = RuntimeSettings::default();
        let is_enabled = |mode, settings| {
            Correction::from_request(
                input::SwitchRequest {
                    previous: None,
                    word: None,
                    target: None,
                    mode,
                },
                settings,
            )
            .is_sound_enabled
        };
        assert!(!is_enabled(SwitchMode::Manual, settings));
        assert!(!is_enabled(SwitchMode::Pattern, settings));
        assert!(!is_enabled(SwitchMode::Auto, settings));
        settings.toggle(RuntimeSetting::ManualSwitchSound);
        assert!(is_enabled(SwitchMode::Manual, settings));
        assert!(!is_enabled(SwitchMode::Pattern, settings));
        assert!(!is_enabled(SwitchMode::Auto, settings));
        settings.toggle(RuntimeSetting::ManualSwitchSound);
        settings.toggle(RuntimeSetting::AutoSwitchSound);
        assert!(!is_enabled(SwitchMode::Manual, settings));
        assert!(is_enabled(SwitchMode::Pattern, settings));
        assert!(is_enabled(SwitchMode::Auto, settings));
    }

    #[test]
    fn checks_manual_replay_for_dead_keys_in_the_next_layout() {
        let correction = Correction {
            mode: SwitchMode::Manual,
            target: None,
            previous: None,
            word: Some(word::Word {
                entries: vec![word::WordStroke {
                    stroke: word::Stroke {
                        key: evdev::KeyCode::KEY_APOSTROPHE,
                        shift: false,
                    },
                    layout: None,
                }],
                ..Default::default()
            }),
            is_sound_enabled: false,
        };
        assert!(correction.has_dead_key_in(&layout::Layout::new("us", "intl", "", "")));
        assert!(!correction.has_dead_key_in(&layout::Layout::new("us", "", "", "")));
    }

    #[test]
    fn cancellation_after_erase_skips_replay() {
        let is_cancelled = AtomicBool::new(false);
        let mut calls = Vec::new();
        replay_correction(
            &mut calls,
            &is_cancelled,
            |calls| {
                calls.push("erase");
                is_cancelled.store(true, Ordering::Relaxed);
                Ok(())
            },
            |calls| {
                calls.push("replay");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls, ["erase"]);
    }

    #[test]
    fn erase_error_skips_replay_and_preserves_error() {
        let error = replay_correction(
            &mut (),
            &AtomicBool::new(false),
            |_| Err(io::Error::other("erase failed")),
            |_| panic!("replay after failed erase"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "erase failed");
    }

    #[test]
    fn records_successful_switch_even_when_cancelled_before_replay() {
        let is_cancelled = AtomicBool::new(false);
        let calls = RefCell::new(Vec::new());
        switch_and_correct::<_, _, io::Error>(
            || {
                calls.borrow_mut().push("switch");
                is_cancelled.store(true, Ordering::Relaxed);
                Ok(())
            },
            || calls.borrow_mut().push("sound"),
            || calls.borrow_mut().push("record"),
            |_| {
                replay_correction(
                    &mut (),
                    &is_cancelled,
                    |_| Ok(()),
                    |_| {
                        calls.borrow_mut().push("replay");
                        Ok(())
                    },
                )
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["switch", "sound", "record"]);
    }

    #[test]
    fn records_before_replay_error_and_skips_record_when_switch_fails() {
        let is_cancelled = AtomicBool::new(false);
        let calls = RefCell::new(Vec::new());
        let result: io::Result<()> = switch_and_correct(
            || {
                calls.borrow_mut().push("switch");
                Ok(())
            },
            || calls.borrow_mut().push("sound"),
            || calls.borrow_mut().push("record"),
            |_| {
                replay_correction(
                    &mut (),
                    &is_cancelled,
                    |_| {
                        calls.borrow_mut().push("erase");
                        Ok(())
                    },
                    |_| {
                        calls.borrow_mut().push("replay");
                        Err(io::Error::other("replay failed"))
                    },
                )
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "replay failed");
        assert_eq!(
            *calls.borrow(),
            ["switch", "sound", "record", "erase", "replay"]
        );
        calls.borrow_mut().clear();
        let result: io::Result<()> = switch_and_correct(
            || Err(io::Error::other("switch failed")),
            || calls.borrow_mut().push("sound"),
            || calls.borrow_mut().push("record"),
            |_| {
                replay_correction(
                    &mut (),
                    &is_cancelled,
                    |_| Ok(()),
                    |_| {
                        calls.borrow_mut().push("replay");
                        Ok(())
                    },
                )
            },
        );
        assert!(result.is_err());
        assert!(calls.borrow().is_empty());
    }
}
