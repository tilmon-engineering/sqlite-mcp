//! Core-wide admission state for ordinary and transient operations.
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionState {
    Running,
    ShuttingDown,
    ShutdownComplete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReservationKind {
    #[allow(dead_code)]
    Ordinary,
    TransientMerge,
}

#[derive(Debug)]
pub(crate) struct Reservation {
    pub(crate) id: u64,
    #[allow(dead_code)]
    pub(crate) kind: ReservationKind,
    pub(crate) cancel: CancellationToken,
}

#[derive(Debug)]
pub(crate) struct AdmissionRegistry {
    state: AdmissionState,
    next_id: u64,
    active: Vec<Arc<Reservation>>,
}

impl Default for AdmissionRegistry {
    fn default() -> Self {
        Self {
            state: AdmissionState::Running,
            next_id: 1,
            active: Vec::new(),
        }
    }
}

impl AdmissionRegistry {
    pub(crate) fn admit(&mut self, kind: ReservationKind) -> Result<Arc<Reservation>, ()> {
        if self.state != AdmissionState::Running {
            return Err(());
        }
        let reservation = Arc::new(Reservation {
            id: self.next_id,
            kind,
            cancel: CancellationToken::new(),
        });
        self.next_id = self.next_id.saturating_add(1);
        self.active.push(reservation.clone());
        Ok(reservation)
    }

    pub(crate) fn finish(&mut self, id: u64) {
        self.active.retain(|entry| entry.id != id);
        if self.state == AdmissionState::ShuttingDown && self.active.is_empty() {
            self.state = AdmissionState::ShutdownComplete;
        }
    }

    pub(crate) fn begin_shutdown(&mut self) -> Vec<Arc<Reservation>> {
        if self.state == AdmissionState::Running {
            self.state = AdmissionState::ShuttingDown;
        }
        for entry in &self.active {
            entry.cancel.cancel();
        }
        self.active.clone()
    }

    pub(crate) fn complete_shutdown(&mut self) {
        if self.active.is_empty() {
            self.state = AdmissionState::ShutdownComplete;
        }
    }

    #[allow(dead_code)]
    pub(crate) fn state(&self) -> AdmissionState {
        self.state
    }

    pub(crate) fn active_len(&self) -> usize {
        self.active.len()
    }
}

pub(crate) type SharedAdmission = Arc<Mutex<AdmissionRegistry>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_rejects_new_admission_and_finishes_after_drain() {
        let mut registry = AdmissionRegistry::default();
        let first = registry.admit(ReservationKind::TransientMerge).unwrap();
        assert!(registry.admit(ReservationKind::Ordinary).is_ok());
        let entries = registry.begin_shutdown();
        assert_eq!(entries.len(), 2);
        assert!(first.cancel.is_cancelled());
        assert!(registry.admit(ReservationKind::Ordinary).is_err());
        let ids: Vec<_> = entries.iter().map(|entry| entry.id).collect();
        for id in ids {
            registry.finish(id);
        }
        assert_eq!(registry.state(), AdmissionState::ShutdownComplete);
    }
}
