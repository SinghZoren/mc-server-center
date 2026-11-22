//! Server instance lifecycle state machine.
//!
//! [`transition`] is a pure function from `(state, event)` to either a new state plus the
//! side effects the engine must perform, an idempotent no-op, or a rejection. Keeping it pure
//! lets us property-test the safety rules that matter most:
//!
//! * a process is only ever killed after the user explicitly confirmed a force kill;
//! * a process in the [`InstanceState::Unknown`] state is never touched (it may not be ours);
//! * repeating a command (`Start` twice, `Stop` twice) is harmless.
//!
//! The diagram lives in `planning/03-system-architecture.md` §3.2.

use serde::{Deserialize, Serialize};

/// Lifecycle state of a server instance as tracked by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    /// Server software is being installed into a staging directory.
    Installing,
    /// The last install attempt failed; the instance can be reinstalled.
    InstallFailed,
    /// Installed and not running.
    Stopped,
    /// Process spawned, waiting for the server to report it is ready.
    Starting,
    /// Server is ready and accepting players.
    Running,
    /// A graceful `stop` was sent; waiting for the process to exit.
    Stopping,
    /// The graceful stop did not finish in time; waiting for the user to decide on a force kill.
    StopTimeout,
    /// A force kill was requested and confirmed; waiting for the process to exit.
    Killing,
    /// The process exited unexpectedly.
    Crashed,
    /// The daemon restarted and is checking whether a previously started process is still ours.
    Reconciling,
    /// The process is ours and alive, but its console is not attached (graceful stop impossible).
    RunningDetached,
    /// A process may exist but its identity could not be verified. Never touched automatically.
    Unknown,
}

impl InstanceState {
    /// Every state, for exhaustive tests.
    pub const ALL: &'static [InstanceState] = &[
        Self::Installing,
        Self::InstallFailed,
        Self::Stopped,
        Self::Starting,
        Self::Running,
        Self::Stopping,
        Self::StopTimeout,
        Self::Killing,
        Self::Crashed,
        Self::Reconciling,
        Self::RunningDetached,
        Self::Unknown,
    ];

    /// Whether a server process is (believed to be) alive in this state.
    #[must_use]
    pub const fn has_live_process(self) -> bool {
        matches!(
            self,
            Self::Starting
                | Self::Running
                | Self::Stopping
                | Self::StopTimeout
                | Self::Killing
                | Self::RunningDetached
        )
    }
}

/// The outcome reported by reconciliation after a daemon restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileOutcome {
    /// Process identity verified and console re-attached.
    Running,
    /// Process identity verified but the console could not be re-attached.
    RunningDetached,
    /// No process is running.
    Stopped,
    /// An install was interrupted and has been rolled back.
    InstallFailed,
    /// A process may exist but could not be verified as ours.
    Unknown,
}

impl ReconcileOutcome {
    const fn state(self) -> InstanceState {
        match self {
            Self::Running => InstanceState::Running,
            Self::RunningDetached => InstanceState::RunningDetached,
            Self::Stopped => InstanceState::Stopped,
            Self::InstallFailed => InstanceState::InstallFailed,
            Self::Unknown => InstanceState::Unknown,
        }
    }
}

/// Something that happened to an instance: a user command or an observation by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InstanceEvent {
    /// User asked to (re)install the server software.
    InstallRequested,
    /// The staged install was promoted successfully.
    InstallSucceeded,
    /// The install failed and was rolled back.
    InstallFailed,
    /// User asked to start the server.
    StartRequested,
    /// The server reported it is ready (log line or status ping).
    ReadyDetected,
    /// User asked for a graceful stop.
    StopRequested,
    /// The graceful-stop timer expired before the process exited.
    StopTimedOut,
    /// User explicitly confirmed killing the process.
    ForceKillConfirmed,
    /// The server process exited. `code` is `None` when it was killed by a signal.
    ProcessExited {
        /// Exit code, if the OS reported one.
        code: Option<i32>,
    },
    /// The daemon restarted; previous process state must be re-verified.
    DaemonRestarted,
    /// User asked to re-run reconciliation (e.g. after resolving an `Unknown` state).
    ReconcileRequested,
    /// Reconciliation finished.
    Reconciled {
        /// What reconciliation found.
        outcome: ReconcileOutcome,
    },
}

/// A side effect the engine must perform after a state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Run the install pipeline.
    BeginInstall,
    /// Spawn the server process from the instance's launch spec.
    SpawnProcess,
    /// Write `stop` to the server console.
    SendStopCommand,
    /// Arm the graceful-stop timeout.
    StartStopTimer,
    /// Cancel the graceful-stop timeout.
    CancelStopTimer,
    /// Kill the verified server process.
    KillProcess,
    /// Record a crash and collect diagnostic evidence.
    RecordCrash,
    /// Verify process identity and re-attach the console.
    Reconcile,
}

/// Why an event did not change anything. Returned so commands are idempotent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoOpReason {
    /// Start requested but the server is already starting or running.
    AlreadyRunning,
    /// Stop requested but the server is not running.
    AlreadyStopped,
    /// Stop requested while a stop or kill is already in progress.
    AlreadyStopping,
    /// Install requested while an install is in progress.
    AlreadyInstalling,
    /// Reconcile requested while reconciliation is in progress.
    AlreadyReconciling,
}

/// Result of a successful [`transition`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// The state changed; perform `effects` in order.
    Changed {
        /// The new state.
        to: InstanceState,
        /// Side effects to perform, in order.
        effects: Vec<Effect>,
    },
    /// Nothing to do; the request was already satisfied or in progress.
    NoOp(NoOpReason),
}

/// Why an event was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    /// The event is not valid in the current state.
    #[error("event {event:?} is not allowed in state {from:?}")]
    NotAllowed {
        /// Current state.
        from: InstanceState,
        /// Rejected event.
        event: InstanceEvent,
    },
    /// A graceful stop needs the console, which is not attached in this state.
    #[error("graceful stop is unavailable in state {from:?}: console not attached")]
    ConsoleUnavailable {
        /// Current state.
        from: InstanceState,
    },
}

impl TransitionError {
    /// The stable error code for this rejection.
    #[must_use]
    pub const fn code(&self) -> crate::ErrorCode {
        match self {
            Self::NotAllowed { .. } => crate::ErrorCode::InvalidStateTransition,
            Self::ConsoleUnavailable { .. } => crate::ErrorCode::ConsoleUnavailable,
        }
    }
}

fn changed(to: InstanceState, effects: &[Effect]) -> Result<Transition, TransitionError> {
    Ok(Transition::Changed {
        to,
        effects: effects.to_vec(),
    })
}

/// Computes the next state for `event` in state `from`.
///
/// # Errors
/// Returns [`TransitionError`] when the event is not permitted in the current state.
pub fn transition(
    from: InstanceState,
    event: InstanceEvent,
) -> Result<Transition, TransitionError> {
    use Effect as E;
    use InstanceEvent as Ev;
    use InstanceState as S;

    match (from, event) {
        // A daemon restart always forces re-verification, whatever we believed before.
        (_, Ev::DaemonRestarted) => changed(S::Reconciling, &[E::Reconcile]),

        // --- install ---
        (S::Stopped | S::InstallFailed | S::Crashed, Ev::InstallRequested) => {
            changed(S::Installing, &[E::BeginInstall])
        }
        (S::Installing, Ev::InstallRequested) => {
            Ok(Transition::NoOp(NoOpReason::AlreadyInstalling))
        }
        (S::Installing, Ev::InstallSucceeded) => changed(S::Stopped, &[]),
        (S::Installing, Ev::InstallFailed) => changed(S::InstallFailed, &[]),

        // --- start ---
        (S::Stopped | S::Crashed, Ev::StartRequested) => changed(S::Starting, &[E::SpawnProcess]),
        (S::Starting | S::Running | S::RunningDetached, Ev::StartRequested) => {
            Ok(Transition::NoOp(NoOpReason::AlreadyRunning))
        }
        (S::Starting, Ev::ReadyDetected) => changed(S::Running, &[]),
        // A duplicate ready signal (log line and status ping both fire) is harmless.
        (S::Running, Ev::ReadyDetected) => changed(S::Running, &[]),

        // --- graceful stop ---
        (S::Starting | S::Running, Ev::StopRequested) => {
            changed(S::Stopping, &[E::SendStopCommand, E::StartStopTimer])
        }
        (S::Stopping | S::StopTimeout | S::Killing, Ev::StopRequested) => {
            Ok(Transition::NoOp(NoOpReason::AlreadyStopping))
        }
        (S::Stopped | S::Crashed | S::InstallFailed, Ev::StopRequested) => {
            Ok(Transition::NoOp(NoOpReason::AlreadyStopped))
        }
        (S::RunningDetached, Ev::StopRequested) => {
            Err(TransitionError::ConsoleUnavailable { from })
        }
        (S::Stopping, Ev::StopTimedOut) => changed(S::StopTimeout, &[]),

        // --- force kill: only ever on explicit confirmation, never in Unknown ---
        (
            S::Starting | S::Running | S::Stopping | S::StopTimeout | S::RunningDetached,
            Ev::ForceKillConfirmed,
        ) => changed(S::Killing, &[E::CancelStopTimer, E::KillProcess]),
        (S::Killing, Ev::ForceKillConfirmed) => Ok(Transition::NoOp(NoOpReason::AlreadyStopping)),

        // --- process exit ---
        // While starting, any exit means the server failed to come up.
        (S::Starting, Ev::ProcessExited { .. }) => changed(S::Crashed, &[E::RecordCrash]),
        // A clean exit while running is a `stop` typed into the console; anything else crashed.
        (S::Running | S::RunningDetached, Ev::ProcessExited { code: Some(0) }) => {
            changed(S::Stopped, &[])
        }
        (S::Running | S::RunningDetached, Ev::ProcessExited { .. }) => {
            changed(S::Crashed, &[E::RecordCrash])
        }
        (S::Stopping | S::StopTimeout, Ev::ProcessExited { .. }) => {
            changed(S::Stopped, &[E::CancelStopTimer])
        }
        (S::Killing, Ev::ProcessExited { .. }) => changed(S::Stopped, &[]),

        // --- reconciliation ---
        (S::Reconciling, Ev::Reconciled { outcome }) => changed(outcome.state(), &[]),
        (S::Reconciling, Ev::ReconcileRequested) => {
            Ok(Transition::NoOp(NoOpReason::AlreadyReconciling))
        }
        (S::Unknown, Ev::ReconcileRequested) => changed(S::Reconciling, &[E::Reconcile]),

        _ => Err(TransitionError::NotAllowed { from, event }),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use InstanceEvent as Ev;
    use InstanceState as S;

    fn to(from: S, ev: Ev) -> S {
        match transition(from, ev).unwrap() {
            Transition::Changed { to, .. } => to,
            Transition::NoOp(_) => from,
        }
    }

    fn effects(from: S, ev: Ev) -> Vec<Effect> {
        match transition(from, ev).unwrap() {
            Transition::Changed { effects, .. } => effects,
            Transition::NoOp(_) => vec![],
        }
    }

    #[test]
    fn happy_path_install_start_stop() {
        let mut s = S::Installing;
        s = to(s, Ev::InstallSucceeded);
        assert_eq!(s, S::Stopped);
        assert_eq!(effects(s, Ev::StartRequested), vec![Effect::SpawnProcess]);
        s = to(s, Ev::StartRequested);
        s = to(s, Ev::ReadyDetected);
        assert_eq!(s, S::Running);
        assert_eq!(
            effects(s, Ev::StopRequested),
            vec![Effect::SendStopCommand, Effect::StartStopTimer]
        );
        s = to(s, Ev::StopRequested);
        s = to(s, Ev::ProcessExited { code: Some(0) });
        assert_eq!(s, S::Stopped);
    }

    #[test]
    fn stop_timeout_then_confirmed_kill() {
        let s = to(S::Stopping, Ev::StopTimedOut);
        assert_eq!(s, S::StopTimeout);
        assert!(effects(s, Ev::ForceKillConfirmed).contains(&Effect::KillProcess));
        let s = to(s, Ev::ForceKillConfirmed);
        assert_eq!(to(s, Ev::ProcessExited { code: None }), S::Stopped);
    }

    #[test]
    fn exits_while_running_distinguish_console_stop_from_crash() {
        assert_eq!(
            to(S::Running, Ev::ProcessExited { code: Some(0) }),
            S::Stopped
        );
        assert_eq!(
            to(S::Running, Ev::ProcessExited { code: Some(1) }),
            S::Crashed
        );
        assert_eq!(to(S::Running, Ev::ProcessExited { code: None }), S::Crashed);
        assert_eq!(
            to(S::Starting, Ev::ProcessExited { code: Some(0) }),
            S::Crashed
        );
    }

    #[test]
    fn repeated_commands_are_no_ops() {
        assert_eq!(
            transition(S::Running, Ev::StartRequested).unwrap(),
            Transition::NoOp(NoOpReason::AlreadyRunning)
        );
        assert_eq!(
            transition(S::Stopped, Ev::StopRequested).unwrap(),
            Transition::NoOp(NoOpReason::AlreadyStopped)
        );
        assert_eq!(
            transition(S::Stopping, Ev::StopRequested).unwrap(),
            Transition::NoOp(NoOpReason::AlreadyStopping)
        );
    }

    #[test]
    fn detached_server_cannot_be_stopped_gracefully() {
        let err = transition(S::RunningDetached, Ev::StopRequested).unwrap_err();
        assert_eq!(err.code(), crate::ErrorCode::ConsoleUnavailable);
    }

    #[test]
    fn unknown_state_never_allows_kill_or_start() {
        assert!(transition(S::Unknown, Ev::ForceKillConfirmed).is_err());
        assert!(transition(S::Unknown, Ev::StartRequested).is_err());
        assert_eq!(to(S::Unknown, Ev::ReconcileRequested), S::Reconciling);
    }

    #[test]
    fn daemon_restart_always_reconciles() {
        for &s in S::ALL {
            assert_eq!(to(s, Ev::DaemonRestarted), S::Reconciling);
            assert_eq!(effects(s, Ev::DaemonRestarted), vec![Effect::Reconcile]);
        }
    }

    #[test]
    fn reconcile_outcomes_map_to_states() {
        let cases = [
            (ReconcileOutcome::Running, S::Running),
            (ReconcileOutcome::RunningDetached, S::RunningDetached),
            (ReconcileOutcome::Stopped, S::Stopped),
            (ReconcileOutcome::InstallFailed, S::InstallFailed),
            (ReconcileOutcome::Unknown, S::Unknown),
        ];
        for (outcome, expected) in cases {
            assert_eq!(to(S::Reconciling, Ev::Reconciled { outcome }), expected);
        }
    }

    #[test]
    fn cannot_start_while_installing() {
        assert!(transition(S::Installing, Ev::StartRequested).is_err());
    }

    fn any_state() -> impl Strategy<Value = S> {
        proptest::sample::select(S::ALL)
    }

    fn any_event() -> impl Strategy<Value = Ev> {
        let outcomes = proptest::sample::select(
            &[
                ReconcileOutcome::Running,
                ReconcileOutcome::RunningDetached,
                ReconcileOutcome::Stopped,
                ReconcileOutcome::InstallFailed,
                ReconcileOutcome::Unknown,
            ][..],
        );
        prop_oneof![
            Just(Ev::InstallRequested),
            Just(Ev::InstallSucceeded),
            Just(Ev::InstallFailed),
            Just(Ev::StartRequested),
            Just(Ev::ReadyDetected),
            Just(Ev::StopRequested),
            Just(Ev::StopTimedOut),
            Just(Ev::ForceKillConfirmed),
            proptest::option::of(any::<i32>()).prop_map(|code| Ev::ProcessExited { code }),
            Just(Ev::DaemonRestarted),
            Just(Ev::ReconcileRequested),
            outcomes.prop_map(|outcome| Ev::Reconciled { outcome }),
        ]
    }

    proptest! {
        /// Safety rule: killing requires an explicit user confirmation, and never targets an
        /// unverified (`Unknown`) process.
        #[test]
        fn kill_only_on_explicit_confirmation(from in any_state(), ev in any_event()) {
            if let Ok(Transition::Changed { effects, .. }) = transition(from, ev)
                && effects.contains(&Effect::KillProcess)
            {
                    prop_assert_eq!(ev, Ev::ForceKillConfirmed);
                    prop_assert!(from != S::Unknown);
                    prop_assert!(from.has_live_process());
            }
        }

        /// Spawning only happens from a state with no live process.
        #[test]
        fn spawn_only_without_live_process(from in any_state(), ev in any_event()) {
            if let Ok(Transition::Changed { effects, .. }) = transition(from, ev)
                && effects.contains(&Effect::SpawnProcess)
            {
                    prop_assert!(!from.has_live_process());
                    prop_assert!(from != S::Unknown);
            }
        }

        /// Applying a user command twice in a row never produces new effects the second time,
        /// except for kill (which remains NoOp in Killing) — commands are idempotent.
        #[test]
        fn user_commands_are_idempotent(from in any_state()) {
            for cmd in [Ev::StartRequested, Ev::StopRequested, Ev::InstallRequested, Ev::ForceKillConfirmed] {
                if let Ok(Transition::Changed { to: next, .. }) = transition(from, cmd) {
                    match transition(next, cmd) {
                        Ok(Transition::NoOp(_)) | Err(_) => {}
                        Ok(Transition::Changed { to, effects }) => {
                            prop_assert!(false, "{cmd:?} twice from {from:?} changed again to {to:?} with {effects:?}");
                        }
                    }
                }
            }
        }

        /// Arbitrary event sequences never panic and always stay within known states.
        #[test]
        fn random_walks_stay_valid(events in proptest::collection::vec(any_event(), 0..64)) {
            let mut s = S::Installing;
            for ev in events {
                if let Ok(Transition::Changed { to, .. }) = transition(s, ev) {
                    s = to;
                }
                prop_assert!(S::ALL.contains(&s));
            }
        }
    }
}
