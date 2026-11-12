use std::sync::Arc;

use crate::layout::Layout;

#[derive(Default)]
pub(super) enum PendingSwitch {
    #[default]
    Idle,
    Manual {
        source: usize,
        is_original_key_held: bool,
    },
    Auto {
        source: usize,
        target: Arc<Layout>,
    },
}

impl PendingSwitch {
    pub(super) fn manual(source: usize) -> Self {
        Self::Manual {
            source,
            is_original_key_held: true,
        }
    }

    pub(super) fn source(&self) -> Option<usize> {
        match self {
            Self::Idle => None,
            Self::Manual { source, .. } | Self::Auto { source, .. } => Some(*source),
        }
    }

    pub(super) fn is_none(&self) -> bool {
        self.source().is_none()
    }
    pub(super) fn is_some(&self) -> bool {
        self.source().is_some()
    }
    pub(super) fn is_auto(&self) -> bool {
        matches!(self, Self::Auto { .. })
    }
    pub(super) fn is_original_key_held(&self) -> bool {
        matches!(
            self,
            Self::Manual {
                is_original_key_held: true,
                ..
            }
        )
    }

    pub(super) fn target(&self) -> Option<Arc<Layout>> {
        match self {
            Self::Auto { target, .. } => Some(target.clone()),
            _ => None,
        }
    }

    pub(super) fn cancel_auto(&mut self) {
        if self.is_auto() {
            self.cancel();
        }
    }

    pub(super) fn update_auto(&mut self, source: usize, target: Option<Arc<Layout>>) {
        if let Some(target) = target {
            *self = Self::Auto { source, target };
        } else {
            self.cancel_auto();
        }
    }

    pub(super) fn release_original_key(&mut self) {
        if let Self::Manual {
            is_original_key_held,
            ..
        } = self
        {
            *is_original_key_held = false;
        }
    }

    pub(super) fn cancel(&mut self) {
        *self = Self::Idle;
    }
}
