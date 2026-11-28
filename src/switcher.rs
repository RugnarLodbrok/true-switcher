use crate::config::RuntimeSettings;
use crate::lifecycle::{ProcessingError, ProcessingResult, ShutdownReason};
use crate::{input, kde, layout, output, sound, statistics, word};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Instant, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum SwitchMode {
    Manual = 0,
    Pattern = 1,
    Auto = 2,
}

struct Correction {
    previous: Option<word::Word>,
    words: Vec<word::Word>,
    target: Option<Arc<layout::Layout>>,
    mode: SwitchMode,
    is_sound_enabled: bool,
    is_learning: bool,
    triggered_at: Option<Instant>,
}

impl Correction {
    fn from_request(request: input::SwitchRequest, settings: RuntimeSettings) -> Self {
        Self {
            previous: request.previous,
            words: request.words,
            target: request.target,
            mode: request.mode,
            is_learning: request.is_learning,
            triggered_at: request.triggered_at,
            is_sound_enabled: match request.mode {
                SwitchMode::Pattern | SwitchMode::Auto => settings.is_auto_switch_sound,
                SwitchMode::Manual => settings.is_manual_switch_sound,
            },
        }
    }

    fn has_dead_key_in(&self, layout: &layout::Layout) -> bool {
        self.words.iter().any(|word| word.has_dead_key_in(layout))
    }

    fn log_timing(&self, result: &ProcessingResult<CorrectionOutcome>) {
        let (
            Some(triggered_at),
            Ok(CorrectionOutcome::Completed {
                started_at,
                finished_at,
            }),
        ) = (self.triggered_at, result)
        else {
            return;
        };
        const MILLIS_PER_SECOND: f64 = 1_000.0;
        let decision_ms = started_at.duration_since(triggered_at).as_secs_f64() * MILLIS_PER_SECOND;
        let correction_ms =
            finished_at.duration_since(*started_at).as_secs_f64() * MILLIS_PER_SECOND;
        log::info!(
            "correction timing: decision_ms={decision_ms:.1} correction_ms={correction_ms:.1}"
        );
    }

    fn learned_word(
        &self,
        outcome: &CorrectionOutcome,
        layout: &layout::Layout,
    ) -> Result<Option<String>, crate::xkb::ConversionError> {
        if !self.is_learning || !matches!(outcome, CorrectionOutcome::Completed { .. }) {
            return Ok(None);
        }
        let Some(word) = self.words.last() else {
            return Ok(None);
        };
        let mut text = layout.to_text(word.search_strokes())?;
        text.truncate(text.trim_end_matches(' ').len());
        Ok((!text.is_empty()).then_some(text))
    }

    fn remember(
        &self,
        input: &mut input::Input,
        outcome: &CorrectionOutcome,
        target: &layout::Layout,
    ) {
        match self.learned_word(outcome, target) {
            Ok(Some(text)) => {
                if let Err(error) = input.remember_word(target.name(), &text) {
                    log::warn!("Cannot save learned correction: {error}");
                }
            }
            Ok(None) => {}
            Err(error) => log::warn!("Cannot convert learned correction: {error}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CorrectionOutcome {
    Completed {
        started_at: Instant,
        finished_at: Instant,
    },
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
        let result = interception.finish().map(|()| outcome);
        correction.log_timing(&result);
        result
    }

    fn begin_and_correct(
        &mut self,
        interception: &mut dyn input::CorrectionInterceptor,
        correction: &Correction,
    ) -> ProcessingResult<CorrectionOutcome> {
        let next_layout = match &correction.target {
            Some(target) if correction.has_dead_key_in(target) => {
                return Ok(CorrectionOutcome::Cancelled);
            }
            None if !correction.words.is_empty() => {
                let (index, layout) = kde::next_layout()?;
                if correction.has_dead_key_in(&layout) || interception.read_available()? {
                    return Ok(CorrectionOutcome::Cancelled);
                }
                Some((index, layout))
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
        let started_at = Instant::now();
        switch_and_correct(
            || match (&correction.target, &next_layout) {
                (Some(target), _) => desktop.switch_to_layout(target.name()),
                (None, Some((index, _))) => desktop.switch_to_layout_index(*index),
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
                    &correction.words,
                    correction.mode,
                )
            },
            |target| {
                interception
                    .input()
                    .begin_correction(correction.words.len(), target.clone());
                let outcome =
                    Self::correct(keyboard, layouts, interception, correction, started_at)?;
                if let Some(target) = target
                    .as_deref()
                    .or_else(|| next_layout.as_ref().map(|(_, layout)| layout))
                {
                    correction.remember(interception.input(), &outcome, target);
                }
                Ok(outcome)
            },
        )
    }

    fn correct(
        keyboard: &mut output::KeyboardOutput,
        layouts: &kde::LayoutTracker,
        interception: &mut dyn input::CorrectionInterceptor,
        correction: &Correction,
        started_at: Instant,
    ) -> ProcessingResult<CorrectionOutcome> {
        let is_cancelled = AtomicBool::new(interception.read_available()?);
        thread::scope(|scope| -> ProcessingResult<CorrectionOutcome> {
            let worker = scope.spawn(|| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    correct_words(correction, keyboard, &is_cancelled)
                }))
                .unwrap_or_else(|_| Err(io::Error::other("correction thread panicked")));
                let finished_at = Instant::now();
                layouts.finish_correction(result);
                finished_at
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
            let finished_at = worker
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
                CorrectionOutcome::Completed {
                    started_at,
                    finished_at,
                }
            })
        })
    }
}

fn correct_words(
    correction: &Correction,
    keyboard: &mut output::KeyboardOutput,
    is_cancelled: &AtomicBool,
) -> io::Result<()> {
    replay_correction(
        keyboard,
        is_cancelled,
        |keyboard| {
            log::debug!("SYNTHETIC erase");
            keyboard.erase(&correction.words, is_cancelled)
        },
        |keyboard| {
            log::debug!("SYNTHETIC replay");
            keyboard.replay(&correction.words, is_cancelled)
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
    words: &[word::Word],
    mode: SwitchMode,
) -> Option<Arc<layout::Layout>> {
    let time = SystemTime::now();
    match kde::current_layout() {
        Ok(target) => {
            statistics.record_switch_with_mode(previous, words, &target, time, mode);
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
    use std::time::Duration;

    #[test]
    fn logs_one_info_timing_per_successful_automatic_correction() {
        use log::LevelFilter;

        let triggered_at = Instant::now();
        let started_at = triggered_at + Duration::from_micros(12_300);
        let finished_at = started_at + Duration::from_micros(23_400);
        let completed = Ok(CorrectionOutcome::Completed {
            started_at,
            finished_at,
        });
        let correction = |mode| {
            Correction::from_request(
                input::SwitchRequest {
                    previous: None,
                    words: vec![],
                    target: None,
                    mode,
                    is_learning: false,
                    triggered_at: (mode != SwitchMode::Manual).then_some(triggered_at),
                },
                RuntimeSettings::default(),
            )
        };

        for mode in [SwitchMode::Auto, SwitchMode::Pattern] {
            let correction = correction(mode);
            let (_, messages) = crate::test_logging::capture_at(LevelFilter::Info, || {
                correction.log_timing(&completed);
            });
            assert_eq!(
                messages,
                ["correction timing: decision_ms=12.3 correction_ms=23.4"]
            );
            let (_, messages) = crate::test_logging::capture_at(LevelFilter::Warn, || {
                correction.log_timing(&completed);
            });
            assert!(messages.is_empty());
        }

        let (_, messages) = crate::test_logging::capture(|| {
            correction(SwitchMode::Manual).log_timing(&completed);
            for result in [
                Ok(CorrectionOutcome::Cancelled),
                Err(ProcessingError::Interrupted),
                Err(io::Error::other("correction failed").into()),
            ] {
                correction(SwitchMode::Auto).log_timing(&result);
            }
        });
        assert!(messages.is_empty());
    }

    #[test]
    fn correction_uses_each_runtime_sound_setting_independently() {
        let mut settings = RuntimeSettings::default();
        let is_enabled = |mode, settings| {
            Correction::from_request(
                input::SwitchRequest {
                    previous: None,
                    words: vec![],
                    target: None,
                    mode,
                    is_learning: false,
                    triggered_at: None,
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
            words: vec![word::Word {
                entries: vec![word::WordStroke {
                    stroke: word::Stroke {
                        key: evdev::KeyCode::KEY_APOSTROPHE,
                        shift: false,
                    },
                    layout: None,
                }],
                ..Default::default()
            }],
            is_sound_enabled: false,
            is_learning: false,
            triggered_at: None,
        };
        assert!(correction.has_dead_key_in(&layout::Layout::new("us", "intl", "", "")));
        assert!(!correction.has_dead_key_in(&layout::Layout::new("us", "", "", "")));
    }

    #[test]
    fn learns_only_successful_replay_in_the_resulting_layout_without_spaces() {
        let mut correction = Correction::from_request(
            input::SwitchRequest {
                previous: None,
                words: vec![word::Word {
                    entries: vec![word::WordStroke {
                        stroke: word::Stroke {
                            key: evdev::KeyCode::KEY_A,
                            shift: false,
                        },
                        layout: Some(Arc::new(layout::Layout::new("us", "", "", ""))),
                    }],
                    trailing_spaces: 2,
                    ..Default::default()
                }],
                target: None,
                mode: SwitchMode::Manual,
                is_learning: true,
                triggered_at: None,
            },
            RuntimeSettings::default(),
        );
        let target = layout::Layout::new("ru", "", "", "");
        let now = Instant::now();
        let completed = CorrectionOutcome::Completed {
            started_at: now,
            finished_at: now,
        };
        assert_eq!(
            correction
                .learned_word(&completed, &target)
                .unwrap()
                .as_deref(),
            Some("ф")
        );
        assert_eq!(
            correction
                .learned_word(&CorrectionOutcome::Cancelled, &target)
                .unwrap(),
            None
        );
        correction.words.clear();
        assert_eq!(correction.learned_word(&completed, &target).unwrap(), None);
    }

    #[test]
    fn teach_trims_strong_strokes_and_keeps_weak_ones() {
        use evdev::KeyCode as K;
        let us = layout::Layout::new("us", "", "pc105", "");
        let ru = Arc::new(layout::Layout::new("ru", "", "pc105", ""));
        let mut punctuation =
            crate::punctuation::Punctuation::new(crate::config::DEFAULT_PUNCTUATION).unwrap();
        punctuation.update_layouts(vec![us.clone(), (*ru).clone()]);
        let now = Instant::now();
        let completed = CorrectionOutcome::Completed {
            started_at: now,
            finished_at: now,
        };
        for (first, is_shift, last, expected) in [
            (K::KEY_SLASH, false, K::KEY_SLASH, "new"),
            (K::KEY_COMMA, true, K::KEY_SLASH, "<new"),
            (K::KEY_SLASH, false, K::KEY_SEMICOLON, "new;"),
        ] {
            let mut history = crate::history::History::default();
            history.process(first, is_shift, Some(ru.clone()), &punctuation);
            for key in [K::KEY_N, K::KEY_E, K::KEY_W, last] {
                history.process(key, false, Some(ru.clone()), &punctuation);
            }
            let correction = Correction::from_request(
                input::SwitchRequest {
                    previous: None,
                    words: history.reset(),
                    target: None,
                    mode: SwitchMode::Manual,
                    is_learning: true,
                    triggered_at: None,
                },
                RuntimeSettings::default(),
            );
            assert_eq!(
                correction.learned_word(&completed, &us).unwrap().as_deref(),
                Some(expected)
            );
        }
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
