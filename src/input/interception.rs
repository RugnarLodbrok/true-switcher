use super::{EntryRecording, Input, is_repair_key};
use crate::lifecycle::ProcessingResult;
use evdev::KeyCode;
use std::collections::BTreeSet;
use std::io;

#[derive(Default)]
pub(super) struct InterceptionState {
    pub(super) recording: Option<EntryRecording>,
    repair: BTreeSet<(usize, u16)>,
    repair_prepared: BTreeSet<(usize, u16)>,
    pub(super) is_captured: bool,
}

#[derive(PartialEq, Eq)]
enum Phase {
    Entering,
    Captured,
    Finishing,
    Finished,
}

pub struct Interceptor<'a> {
    input: &'a mut Input,
    state: InterceptionState,
    phase: Phase,
    is_cleanup_needed: bool,
    is_release_wait_complete: bool,
    is_lock_release_complete: bool,
}

impl<'a> Interceptor<'a> {
    pub fn enter(input: &'a mut Input) -> ProcessingResult<Option<Self>> {
        let mut interception = Self {
            input,
            state: InterceptionState::default(),
            phase: Phase::Entering,
            is_cleanup_needed: false,
            is_release_wait_complete: false,
            is_lock_release_complete: false,
        };
        let mut is_switch_key_reset = false;
        let mut attempt = 0_u64;
        loop {
            if !interception.wait_ordinary()? {
                interception.rollback()?;
                interception.phase = Phase::Finished;
                return Ok(None);
            }
            attempt += 1;
            log::debug!("ENTRY attempt={attempt} begin");
            interception.begin_entry(&mut is_switch_key_reset)?;
            let presses = interception.state.recording.take().unwrap().presses;
            log::trace!("ENTRY attempt={attempt} recorded {presses:?}");
            if interception.input.pending.is_none() {
                interception.state.repair = presses;
                interception.rollback()?;
                interception.phase = Phase::Finished;
                return Ok(None);
            }
            if presses.is_empty() {
                interception.state.is_captured = true;
                interception.phase = Phase::Captured;
                interception.input.is_correction_cancelled = false;
                interception.input.is_correcting = true;
                interception.input.pending = None;
                log::debug!("ENTRY accepted");
                return Ok(Some(interception));
            }
            interception.state.repair = presses;
            interception.rollback()?;
            interception.is_cleanup_needed = false;
        }
    }

    fn wait_ordinary(&mut self) -> ProcessingResult<bool> {
        log::trace!("WAIT_ORDINARY begin");
        self.input
            .process_events_until(|input| !input.is_ordinary_key_held(), Some(&mut self.state))?;
        Ok(self.input.pending.is_some())
    }

    fn begin_entry(&mut self, is_switch_key_reset: &mut bool) -> ProcessingResult<()> {
        self.is_cleanup_needed = true;
        self.is_lock_release_complete = false;
        self.state.recording = Some(EntryRecording::default());
        self.input.release_locks()?;
        self.input.release_modifiers()?;
        if !*is_switch_key_reset {
            let source = self.input.pending.unwrap();
            if self.input.keyboards[source]
                .held
                .contains(&self.input.switch_key.0)
            {
                self.input.write_source(source, self.input.switch_key, 0)?;
            }
            *is_switch_key_reset = true;
            self.input.is_original_switch_key_held = false;
        }
        self.process_available_events()?;
        self.input.grab()?;
        self.process_available_events()?;
        self.input.snapshot(Some(&mut self.state))?;
        self.process_available_events()
    }

    #[cfg(test)]
    pub fn input(&self) -> &Input {
        self.input
    }

    pub fn read_captured(&mut self) -> ProcessingResult<bool> {
        self.process_available_events()?;
        Ok(self.take_cancel_request())
    }

    pub fn wait_for_correction(&mut self) -> ProcessingResult<Option<io::Result<()>>> {
        self.input.wait_for_correction(Some(&mut self.state))
    }

    pub fn take_cancel_request(&mut self) -> bool {
        self.input.take_cancel_request()
    }

    fn process_available_events(&mut self) -> ProcessingResult<()> {
        self.input.process_available_events(Some(&mut self.state))
    }

    fn process_cleanup_events(&mut self) -> ProcessingResult<()> {
        self.input.process_cleanup_events(Some(&mut self.state))
    }

    pub fn finish(mut self) -> ProcessingResult<()> {
        self.phase = Phase::Finishing;
        self.wait_releases()?;
        self.is_release_wait_complete = true;
        self.input.ungrab()?;
        self.state.is_captured = false;
        self.release_locks()?;
        self.process_available_events()?;
        self.phase = Phase::Finished;
        log::trace!("WAIT_RELEASES complete");
        Ok(())
    }

    fn wait_releases(&mut self) -> ProcessingResult<()> {
        log::trace!("WAIT_RELEASES begin");
        loop {
            self.process_available_events()?;
            self.input.release_modifiers()?;
            self.process_available_events()?;
            if self
                .input
                .keyboards
                .iter()
                .all(|keyboard| keyboard.held.is_empty())
            {
                return Ok(());
            }
            self.input.wait_for_events(None, Some(&mut self.state))?;
        }
    }

    fn release_locks(&mut self) -> ProcessingResult<()> {
        if !self.is_lock_release_complete {
            self.input.release_locks()?;
            self.is_lock_release_complete = true;
        }
        Ok(())
    }

    fn prepare_repair(&mut self) -> ProcessingResult<()> {
        let mut error = None;
        for (index, code) in self.state.repair.clone() {
            if !is_repair_key(KeyCode::new(code)) {
                self.state.repair.remove(&(index, code));
                continue;
            }
            if !self.input.keyboards[index].device.is_grabbed() {
                if !self.state.repair_prepared.contains(&(index, code)) {
                    self.state.repair.remove(&(index, code));
                }
                continue;
            }
            let is_preparation_needed = self.state.repair_prepared.insert((index, code));
            if is_preparation_needed
                && let Err(err) = self.input.write_source(index, KeyCode::new(code), 1)
            {
                log::error!("ROLLBACK prepare keyboard={index} code={code}: {err}");
                error.get_or_insert(err);
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn repair_releases(&mut self) -> ProcessingResult<()> {
        let mut error = None;
        for (index, code) in self.state.repair.clone() {
            if !self.input.keyboards[index].device.is_grabbed() {
                match self.input.write_source(index, KeyCode::new(code), 0) {
                    Ok(()) => {
                        self.state.repair.remove(&(index, code));
                        self.state.repair_prepared.remove(&(index, code));
                    }
                    Err(err) => {
                        log::error!("ROLLBACK release keyboard={index} code={code}: {err}");
                        error.get_or_insert(err);
                    }
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn rollback(&mut self) -> ProcessingResult<()> {
        if !self.is_cleanup_needed {
            return Ok(());
        }
        log::debug!("ROLLBACK begin");
        log::trace!("ROLLBACK keys {:?}", self.state.repair);
        let mut error = None;
        for (stage, result) in [
            ("prepare", self.prepare_repair()),
            ("events", self.process_cleanup_events()),
            ("ungrab", self.input.ungrab()),
            ("release", self.repair_releases()),
            ("locks", self.release_locks()),
        ] {
            if let Err(err) = result {
                log::error!("ROLLBACK {stage}: {err}");
                error.get_or_insert(err);
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn collect_recording(&mut self) {
        if self.state.recording.is_none() {
            return;
        }
        Self::log_cleanup("events", self.process_cleanup_events());
        Self::log_cleanup("snapshot", self.input.snapshot(Some(&mut self.state)));
        self.state
            .repair
            .extend(self.state.recording.take().unwrap().presses);
    }

    fn log_cleanup(stage: &str, result: ProcessingResult<()>) {
        if let Err(error) = result {
            log::error!("CLEANUP {stage}: {error}");
        }
    }
}

impl Drop for Interceptor<'_> {
    fn drop(&mut self) {
        self.input.is_correcting = false;
        if self.phase == Phase::Finished || !self.is_cleanup_needed {
            return;
        }
        log::debug!("CLEANUP begin");
        if self.phase == Phase::Entering {
            self.collect_recording();
            Self::log_cleanup("rollback", self.rollback());
        }
        if self.state.is_captured && !self.is_release_wait_complete {
            Self::log_cleanup("modifiers", self.input.release_modifiers());
        }
        Self::log_cleanup("ungrab", self.input.ungrab());
        if self.phase == Phase::Entering {
            Self::log_cleanup("repair releases", self.repair_releases());
        }
        Self::log_cleanup("locks", self.release_locks());
        log::debug!("CLEANUP end");
    }
}

const IS_CAPTURE_ENABLED: bool = false;

pub trait CorrectionInterceptor {
    fn input(&mut self) -> &mut Input;
    fn read_available(&mut self) -> ProcessingResult<bool>;
    fn wait_for_correction(&mut self) -> ProcessingResult<Option<io::Result<()>>>;
    fn finish(self: Box<Self>) -> ProcessingResult<()>;
}

pub fn enter_interceptor(
    input: &mut Input,
) -> ProcessingResult<Option<Box<dyn CorrectionInterceptor + '_>>> {
    if IS_CAPTURE_ENABLED {
        Ok(Interceptor::enter(input)?
            .map(|interceptor| Box::new(interceptor) as Box<dyn CorrectionInterceptor>))
    } else {
        Ok(Some(Box::new(UncapturedInterceptor::enter(input)?)))
    }
}

impl CorrectionInterceptor for Interceptor<'_> {
    fn input(&mut self) -> &mut Input {
        self.input
    }

    fn read_available(&mut self) -> ProcessingResult<bool> {
        self.read_captured()
    }

    fn wait_for_correction(&mut self) -> ProcessingResult<Option<io::Result<()>>> {
        Interceptor::wait_for_correction(self)
    }

    fn finish(self: Box<Self>) -> ProcessingResult<()> {
        Interceptor::finish(*self)
    }
}

pub(super) struct UncapturedInterceptor<'a> {
    input: &'a mut Input,
}

impl<'a> UncapturedInterceptor<'a> {
    pub(super) fn enter(input: &'a mut Input) -> ProcessingResult<Self> {
        input.is_correcting = true;
        input.is_correction_cancelled = false;
        input.pending = None;
        input.is_original_switch_key_held = false;
        let interceptor = Self { input };
        let mut error = None;
        for index in 0..interceptor.input.keyboards.len() {
            for code in interceptor.input.keyboards[index].held.clone() {
                if let Err(err) = interceptor.input.write_source(index, KeyCode::new(code), 0) {
                    error.get_or_insert(err);
                }
            }
        }
        error.map_or(Ok(interceptor), Err)
    }
}

impl CorrectionInterceptor for UncapturedInterceptor<'_> {
    fn input(&mut self) -> &mut Input {
        self.input
    }

    fn read_available(&mut self) -> ProcessingResult<bool> {
        self.input.process_available_events(None)?;
        Ok(self.input.take_cancel_request())
    }

    fn wait_for_correction(&mut self) -> ProcessingResult<Option<io::Result<()>>> {
        self.input.wait_for_correction(None)
    }

    fn finish(self: Box<Self>) -> ProcessingResult<()> {
        Ok(())
    }
}

impl Drop for UncapturedInterceptor<'_> {
    fn drop(&mut self) {
        self.input.is_correcting = false;
    }
}
