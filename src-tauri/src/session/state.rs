//! Session lifecycle states and the transition table
//! (`docs/MVP_IMPLEMENTATION_SPEC.md` §5).
//!
//! This is the one place that decides which lifecycle moves are legal. It is a
//! pure table over the status enum — no I/O, no clock, no process — so the
//! rules Session Core enforces can be read off the spec and tested directly.
//! Session Core asks the table and turns a `false` into a structured
//! rejection; nothing else is allowed to invent a transition.

use serde::Serialize;

/// MVP lifecycle states (spec §4).
///
/// §4 also lists optional `busy`/`ready` runtime flags, and neither is a status
/// here — deliberately, and permanently. Since T08 there *is* a health signal
/// (`SessionRuntime::health`, §12), and `ready` is derived from it on the side
/// that reads it; `busy` has no source at all in the MVP. What matters for this
/// table is the rule they obey: they supplement lifecycle state and never
/// replace it, which is why a service whose port is not listening is still
/// `Running` and no transition exists for "unhealthy".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Exited,
    Error,
}

/// Every status, so callers (and tests) can reason over the whole space.
pub const ALL_STATUSES: [SessionStatus; 6] = [
    SessionStatus::Stopped,
    SessionStatus::Starting,
    SessionStatus::Running,
    SessionStatus::Stopping,
    SessionStatus::Exited,
    SessionStatus::Error,
];

impl SessionStatus {
    /// Literal used in DTOs and event payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Stopped => "stopped",
            SessionStatus::Starting => "starting",
            SessionStatus::Running => "running",
            SessionStatus::Stopping => "stopping",
            SessionStatus::Exited => "exited",
            SessionStatus::Error => "error",
        }
    }

    /// Whether `self -> next` is a transition the MVP state machine allows.
    ///
    /// The allowed set is the one listed in spec §5 and nothing besides:
    ///
    /// ```text
    /// Stopped  -> Starting
    /// Starting -> Running | Error
    /// Running  -> Stopping | Exited | Error
    /// Stopping -> Exited | Stopped | Error
    /// Exited   -> Starting
    /// Error    -> Starting | Stopping
    /// ```
    ///
    /// `Error -> Stopping` is the recovery path, not an ordinary stop: a stop
    /// that could not confirm the tree was gone leaves the run owned by the
    /// session, and the stop ladder is the one action that can still end it.
    /// Without the pair, "force stop" would be unavailable exactly when a
    /// process tree refuses to die.
    ///
    /// Everything else is rejected, including the transitions that look
    /// harmless but are not listed (`Stopped -> Running`, `Started`-while-
    /// `Running`, and so on). A missing pair is a question for the spec, not a
    /// gap for this function to fill in.
    pub fn can_transition_to(self, next: SessionStatus) -> bool {
        use SessionStatus::*;
        matches!(
            (self, next),
            (Stopped, Starting)
                | (Starting, Running | Error)
                | (Running, Stopping | Exited | Error)
                | (Stopping, Exited | Stopped | Error)
                | (Error, Starting | Stopping)
                | (Exited, Starting)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::SessionStatus::*;
    use super::*;

    /// The transitions spec §5 lists, transcribed literally.
    ///
    /// Kept as a flat human-readable list rather than a table so the test
    /// fails to compile — not merely to pass — if a status is added to the
    /// enum without a decision about how it moves.
    const SPEC_ALLOWS: &[(SessionStatus, SessionStatus)] = &[
        (Stopped, Starting),
        (Starting, Running),
        (Running, Stopping),
        (Stopping, Exited),
        (Stopping, Stopped),
        (Running, Exited),
        (Starting, Error),
        (Running, Error),
        (Stopping, Error),
        (Exited, Starting),
        (Error, Starting),
        (Error, Stopping),
    ];

    #[test]
    fn the_only_allowed_transitions_are_the_ones_the_spec_lists() {
        for from in ALL_STATUSES {
            for to in ALL_STATUSES {
                let expected = SPEC_ALLOWS.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "{} -> {} should be {}",
                    from.as_str(),
                    to.as_str(),
                    if expected { "allowed" } else { "rejected" }
                );
            }
        }
    }

    /// The half of "invalid transitions are rejected" that a table can get
    /// wrong on its own: a state must not silently accept itself, or a
    /// duplicate start/stop would read as a legal move rather than a no-op the
    /// caller has to decide about.
    #[test]
    fn no_status_transitions_to_itself() {
        for status in ALL_STATUSES {
            assert!(
                !status.can_transition_to(status),
                "{} -> {} must not be self-allowed",
                status.as_str(),
                status.as_str()
            );
        }
    }

    /// A service that just stopped has to be startable again; that is the
    /// whole point of `Exited`/`Error` being non-terminal in this MVP.
    #[test]
    fn both_ended_states_can_restart() {
        assert!(Exited.can_transition_to(Starting));
        assert!(Error.can_transition_to(Starting));
        assert!(Stopped.can_transition_to(Starting));
    }
}
