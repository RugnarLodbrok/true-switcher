use std::io;

#[derive(Default)]
pub(super) struct CorrectionState {
    is_correcting: bool,
    is_cancel_requested: bool,
    is_correction_cancelled: bool,
    correction_result: Option<io::Result<()>>,
}

impl CorrectionState {
    pub(super) fn activate(&mut self) {
        self.is_correcting = true;
        self.is_correction_cancelled = false;
    }

    pub(super) fn deactivate(&mut self) {
        self.is_correcting = false;
    }

    pub(super) fn is_active(&self) -> bool {
        self.is_correcting
    }

    pub(super) fn request_cancel(&mut self) {
        self.is_cancel_requested = true;
        self.is_correction_cancelled = true;
    }

    pub(super) fn take_cancel_request(&mut self) -> bool {
        std::mem::take(&mut self.is_cancel_requested)
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.is_correction_cancelled
    }

    pub(super) fn store_result(&mut self, result: io::Result<()>) {
        self.correction_result = Some(result);
    }

    pub(super) fn has_result(&self) -> bool {
        self.correction_result.is_some()
    }

    pub(super) fn take_result(&mut self) -> Option<io::Result<()>> {
        self.correction_result.take()
    }
}

#[cfg(test)]
mod tests {
    use super::CorrectionState;
    use std::io;

    #[test]
    fn activation_preserves_pending_notification_and_result() {
        let mut state = CorrectionState::default();
        state.request_cancel();
        state.store_result(Err(io::Error::other("previous correction failed")));

        state.activate();

        assert!(state.is_active());
        assert!(!state.is_cancelled());
        assert!(state.take_cancel_request());
        assert!(!state.take_cancel_request());
        assert_eq!(
            state.take_result().unwrap().unwrap_err().to_string(),
            "previous correction failed"
        );
        assert!(!state.has_result());
        assert!(state.is_active());
    }

    #[test]
    fn deactivation_preserves_cancellation_and_result() {
        let mut state = CorrectionState::default();
        state.activate();
        state.request_cancel();
        state.store_result(Ok(()));

        state.deactivate();

        assert!(!state.is_active());
        assert!(state.take_cancel_request());
        assert!(state.is_cancelled());
        assert!(state.take_result().unwrap().is_ok());
        assert!(state.is_cancelled());
    }
}
