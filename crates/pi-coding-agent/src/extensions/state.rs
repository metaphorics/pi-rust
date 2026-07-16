//! Sidecar bridge lifecycle state machine (Phase 6 plan §4).
//!
//! `NotNeeded` (zero extensions ⇒ zero Bun) → `Detected` → `Spawning` →
//! `Handshaking` → `Loading` → `Ready` ⇄ (`Draining` on shutdown) →
//! `Dead(reason)` → `Respawning` (exactly one attempt per death) → `Disabled`.

use std::fmt;

/// Why the sidecar process (or an attempt to reach it) died.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeadReason {
    /// The Bun process could not be spawned at all.
    SpawnFailed(String),
    /// The first frame was not a valid `lifecycle/hello` (pollution, garbage,
    /// version mismatch, EOF, or timeout before hello).
    HandshakeFailed(String),
    /// `lifecycle/init` failed or `lifecycle/initialized` never arrived.
    LoadFailed(String),
    /// stdout or stdin closed unexpectedly.
    StdioClosed,
    /// The process exited on its own with this code (`None` = killed by signal).
    Exited(Option<i32>),
    /// Heartbeat pings went unanswered past the miss budget.
    HeartbeatMissed,
    /// The host killed the process.
    Killed,
    /// Deliberate host-driven shutdown (terminal; never respawned).
    Shutdown,
}

impl fmt::Display for DeadReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnFailed(error) => write!(f, "sidecar spawn failed: {error}"),
            Self::HandshakeFailed(error) => write!(f, "sidecar handshake failed: {error}"),
            Self::LoadFailed(error) => write!(f, "sidecar load phase failed: {error}"),
            Self::StdioClosed => write!(f, "sidecar stdio closed"),
            Self::Exited(Some(code)) => write!(f, "sidecar exited with code {code}"),
            Self::Exited(None) => write!(f, "sidecar was terminated by a signal"),
            Self::HeartbeatMissed => write!(f, "sidecar stopped answering heartbeats"),
            Self::Killed => write!(f, "sidecar was killed by the host"),
            Self::Shutdown => write!(f, "sidecar was shut down"),
        }
    }
}

/// Why extensions are permanently disabled for this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisabledReason {
    /// Bun or the sidecar entry could not be resolved (decision 7: the
    /// message carries the one exact install command when Bun is missing).
    LaunchUnavailable(String),
    /// The single respawn attempt after a death also failed.
    RespawnFailed(String),
}

impl fmt::Display for DisabledReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LaunchUnavailable(error) => write!(f, "{error}"),
            Self::RespawnFailed(error) => {
                write!(
                    f,
                    "sidecar respawn failed ({error}); extensions stay disabled"
                )
            }
        }
    }
}

/// Host-side view of the sidecar bridge lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeState {
    /// Discovery found zero extensions: no detection, no spawn, ever.
    NotNeeded,
    /// Extensions exist and Bun resolved; nothing spawned yet (lazy).
    Detected,
    /// Process being spawned.
    Spawning,
    /// Waiting for the sidecar's `lifecycle/hello`.
    Handshaking,
    /// `lifecycle/init` in flight (the sidecar loads extensions during init).
    Loading,
    /// Fully operational.
    Ready,
    /// Graceful shutdown in progress.
    Draining,
    /// Process gone.
    Dead(DeadReason),
    /// The one respawn attempt granted by the last death is running.
    Respawning,
    /// Extensions permanently off for this process.
    Disabled(DisabledReason),
}

impl BridgeState {
    /// Whether `next` is a legal successor of `self` (plan §4 arrows).
    pub fn can_transition(&self, next: &BridgeState) -> bool {
        use BridgeState::{
            Dead, Detected, Disabled, Draining, Handshaking, Loading, Ready, Respawning, Spawning,
        };
        matches!(
            (self, next),
            (Detected, Spawning)
                | (Spawning, Handshaking | Dead(_))
                | (Handshaking, Loading | Dead(_))
                | (Loading, Ready | Dead(_))
                | (Ready, Draining | Dead(_))
                | (Draining, Dead(_))
                | (Dead(_), Respawning | Disabled(_))
                | (Respawning, Ready | Disabled(_))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead() -> BridgeState {
        BridgeState::Dead(DeadReason::StdioClosed)
    }

    fn disabled() -> BridgeState {
        BridgeState::Disabled(DisabledReason::LaunchUnavailable("bun missing".to_string()))
    }

    #[test]
    fn happy_path_transitions_are_legal() {
        let path = [
            BridgeState::Detected,
            BridgeState::Spawning,
            BridgeState::Handshaking,
            BridgeState::Loading,
            BridgeState::Ready,
            BridgeState::Draining,
            dead(),
        ];
        for pair in path.windows(2) {
            assert!(
                pair[0].can_transition(&pair[1]),
                "{:?} -> {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn death_and_recovery_transitions() {
        for phase in [
            BridgeState::Spawning,
            BridgeState::Handshaking,
            BridgeState::Loading,
            BridgeState::Ready,
        ] {
            assert!(phase.can_transition(&dead()));
        }
        assert!(dead().can_transition(&BridgeState::Respawning));
        assert!(dead().can_transition(&disabled()));
        assert!(BridgeState::Respawning.can_transition(&BridgeState::Ready));
        assert!(BridgeState::Respawning.can_transition(&disabled()));
    }

    #[test]
    fn terminal_states_have_no_exits() {
        let all = [
            BridgeState::NotNeeded,
            BridgeState::Detected,
            BridgeState::Spawning,
            BridgeState::Handshaking,
            BridgeState::Loading,
            BridgeState::Ready,
            BridgeState::Draining,
            dead(),
            BridgeState::Respawning,
            disabled(),
        ];
        for next in &all {
            assert!(!BridgeState::NotNeeded.can_transition(next));
            assert!(!disabled().can_transition(next));
        }
        // Ready never regresses into the spawn pipeline.
        assert!(!BridgeState::Ready.can_transition(&BridgeState::Spawning));
        assert!(!BridgeState::Ready.can_transition(&BridgeState::Loading));
        // Dead never jumps straight back to Ready without a respawn attempt.
        assert!(!dead().can_transition(&BridgeState::Ready));
    }

    #[test]
    fn launch_unavailable_relays_the_resolver_message() {
        let reason = DisabledReason::LaunchUnavailable("install bun with: curl".to_string());
        assert_eq!(reason.to_string(), "install bun with: curl");
    }
}
