//! Before-sleep locking and the logind session `Lock` signal — the safety
//! core (Stage 4).
//!
//! This module implements Architecture's binding sleep state machine
//! (`PLAN.md`) on top of the logind mechanics Stage 2 verified
//! (`docs/SIGNALS.md` §3). Read those two first; this comment only explains
//! the *shape of the code*, not the decisions, which live there.
//!
//! ```text
//! Awake(inhibitor held) ──PrepareForSleep(true)──▶ LockPending(spawn locker,
//!   ▲                                                await confirmation)
//!   │                                                   │
//!   └──PrepareForSleep(false): re-acquire inhibitor──┐  │ confirmed, or deadline
//!                                                    │  ▼ (whichever first)
//!                                           ReadyToSleep(inhibitor released)
//! ```
//!
//! # The confirmation mechanism (Stage 2's decision, not a fresh one)
//!
//! "The session is locked" means **logind's per-session `LockedHint`
//! property is `true`** — set by niri itself once its lock state reaches
//! `Locked(_)`, i.e. once the lock surfaces are actually up. That is the
//! signal `docs/SIGNALS.md`'s decision section chose, empirically confirmed
//! on this machine (spawn → `LockedHint` observed ≈ 360 ms warm). This
//! module reads it in exactly two places, and nowhere else invents a notion
//! of "locked":
//!
//! 1. [`SessionLocker::lock_if_needed`] — the already-locked check that
//!    stops us stacking a second locker on top of a live one.
//! 2. [`SleepMachine::await_lock_confirmation`] — the `LockPending` wait,
//!    polled every [`CONFIRMATION_POLL_INTERVAL`] until it flips or the
//!    deadline expires.
//!
//! Polling (rather than watching `PropertiesChanged`) is deliberate: the
//! window is at most ~2 s, a poll is one cheap D-Bus round trip, and a poll
//! cannot miss an edge the way a signal subscription that raced the lock
//! could. It is also exactly what Stage 2's confirmed measurement did, so
//! the numbers in `docs/SIGNALS.md` describe the mechanism actually shipped.
//!
//! # The trait boundary (the lockscreen's `Authenticator` pattern)
//!
//! [`Logind`] and [`LockerSpawner`] are the two small traits that stand
//! between the state machine and the outside world. The machine holds
//! `Arc<dyn Logind>` / `Arc<dyn LockerSpawner>` and never touches zbus or
//! `tokio::process` directly, so every test at the bottom of this file
//! drives the *real* machine through fakes — no D-Bus, no child processes,
//! no real clock (`#[tokio::test(start_paused = true)]`). The real
//! implementations ([`LogindConnection`], [`CommandLocker`]) are the only
//! things in here that know what a file descriptor or a `Command` is.
//!
//! Teaching note on the future types: the trait methods return
//! `Pin<Box<dyn Future<...> + Send + '_>>` ([`BoxFuture`]) rather than being
//! `async fn`s. Rust does allow `async fn` in traits now, but a trait with
//! one is not *dyn-compatible* — and `Arc<dyn Logind>` (so tests can swap in
//! a fake at runtime) is the whole point. Boxing the future is the standard
//! price for that; `saola-lockscreen`'s `auth::Authenticator` makes the same
//! trade for the same reason.
//!
//! # Async task ownership
//!
//! [`run`] is one long-lived task owned by `main.rs`'s `JoinSet`. It owns
//! the [`SleepMachine`], which owns the [`Inhibitor`] — and the inhibitor is
//! *a file descriptor whose release is its drop* (`docs/SIGNALS.md` §3:
//! "The lock is released the moment this file descriptor and all its
//! duplicates are closed"). There is no `release()` call to forget to make;
//! there is only a value to keep alive. That is why `main.rs` joins this
//! task before the process exits: returning from [`run`] drops the machine,
//! which drops the fd, which releases the inhibitor — in that order, before
//! the process goes away.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::config::SessionConfig;

/// What we ask logind to inhibit. `sleep` covers suspend, hibernate and
/// hybrid-sleep (`man org.freedesktop.login1`); we deliberately do not
/// inhibit `shutdown` (locking a machine that is powering off is pointless
/// and only delays it) or `idle` (that is niri's business, and Stage 5's).
const INHIBIT_WHAT: &str = "sleep";

/// The `who` string. This is what `systemd-inhibit --list` shows in its WHO
/// column — Stage 4's live check greps for exactly this.
const INHIBIT_WHO: &str = "saola-session";

/// The `why` string, shown by `systemd-inhibit --list` in its WHY column.
const INHIBIT_WHY: &str = "Lock the session before sleep";

/// `delay`, never `block`. A `block` inhibitor would refuse the suspend
/// outright, which is Architecture's severity-2 failure ("suspend blocked or
/// delayed indefinitely") by construction. `delay` asks logind to wait for
/// us, bounded by its own `InhibitDelayMaxUSec`.
const INHIBIT_MODE: &str = "delay";

/// The `LockPending` hard deadline when logind's own cap is unknown.
///
/// Stage 2 measured `InhibitDelayMaxUSec = 5 s` on this machine. Stage 7's
/// review (finding E-6) corrected the reasoning behind this number: the
/// original 2 s figure was chosen to leave headroom for "whatever else
/// holds a delay inhibitor", but that reasoning runs backwards — if another
/// inhibitor is also delaying, logind is waiting for *it* regardless of what
/// we do, and our extra seconds cost nothing; if we are the only inhibitor,
/// the whole cap is ours to spend. Severity rule 1 ("session exposed") is
/// what this deadline actually protects against, and giving back margin
/// against a cold-boot locker (a synchronous wallpaper decode, an
/// uncached binary) buys nothing against severity rule 2, which logind's
/// own cap already makes systemically impossible regardless of this
/// constant. 4 s leaves 1 s of margin for the release itself (a `close()`),
/// which is ample. It is a constant rather than a `session.kdl` knob on
/// purpose: Stage 3's schema is canonical and Stage 8's README documents it
/// verbatim, and this value is not something an operator can usefully tune
/// — it is bounded above by logind's cap and below by how fast the locker
/// can get its surfaces up, neither of which is a matter of taste.
const DEFAULT_CONFIRMATION_DEADLINE: Duration = Duration::from_secs(4);

/// How long after a locker spawn to treat the session as "probably locked"
/// even if `LockedHint` still reads `false` — [`SessionLocker`]'s fix for
/// Stage 7's finding E-3. `docs/SIGNALS.md` §1 measured spawn →
/// `LockedHint == true` at ~360 ms warm; for that whole window, two
/// independent clones of the same `SessionLocker` (idle.rs's task and this
/// module's task both hold one) would each read `LockedHint == false` and
/// each conclude they must spawn, stacking a second locker on top of a
/// live one. Deliberately the same value as [`DEFAULT_CONFIRMATION_DEADLINE`]:
/// that is already this crate's stated bound on how long a lock may take to
/// confirm, so this is not a new number to justify independently.
const LOCK_SETTLE_GRACE: Duration = DEFAULT_CONFIRMATION_DEADLINE;

/// How often [`run`] retries a failed logind connection while degraded
/// (Stage 7's finding E-1), and — the same timer, folded in per the review's
/// suggestion — how often it retries [`SleepMachine::ensure_inhibitor`] when
/// connected but not currently holding one (finding E-5). Deliberately slow:
/// this is "logind is unreachable or briefly refusing us", not a
/// fast-changing condition, and a retry attempt every 30 s is free next to
/// the severity-1 cost of never trying again.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(30);

/// How often the `LockPending` wait re-reads `LockedHint`. 50 ms gives ~40
/// samples inside a 2 s deadline and resolves the confirmation latency finely
/// enough that the duration this module logs on every cycle stays a useful
/// measurement (Stage 2 asked for exactly that, so the ~360 ms figure in
/// `docs/SIGNALS.md` keeps being re-verified in production rather than
/// becoming a one-time assumption).
const CONFIRMATION_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How many times to ask logind for the delay inhibitor before giving up on
/// one acquisition attempt (startup, or a resume). A transient D-Bus hiccup
/// during resume is exactly the moment we cannot afford to silently stop
/// holding it.
const INHIBITOR_ACQUIRE_ATTEMPTS: u32 = 3;

/// Backoff between those attempts. Deliberately short: on the resume path
/// the whole retry budget (2 × 250 ms) has to fit inside the window before
/// the machine could plausibly be asked to sleep again.
const INHIBITOR_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// The boxed-future type every trait method in this module returns. See the
/// module doc comment ("The trait boundary") for why these are boxed futures
/// and not `async fn`s in the trait.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A held logind delay inhibitor.
///
/// Opaque on purpose: **there is no `release()` method because logind has no
/// release call**. Dropping this value drops whatever the implementation put
/// inside it — for [`LogindConnection`] that is the `OwnedFd` logind handed
/// back from `Inhibit()`, and closing it *is* the release. Modelling it as a
/// value with a lifetime rather than a handle with a method means the
/// compiler enforces the one rule that matters: as long as the machine holds
/// this, the machine is delaying sleep.
pub struct Inhibitor {
    /// Never read — only dropped. The underscore keeps `dead_code` quiet
    /// about a field whose entire purpose is its `Drop`.
    _release: Box<dyn Send + Sync>,
}

impl Inhibitor {
    /// Wraps whatever the implementation wants released on drop (a real fd,
    /// or a test double that records the release).
    pub fn new<T: Send + Sync + 'static>(release: T) -> Self {
        Inhibitor {
            _release: Box::new(release),
        }
    }
}

impl fmt::Debug for Inhibitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Inhibitor(held)")
    }
}

/// Anything logind refused or failed to answer. Stringly-typed on purpose:
/// nothing in this module branches on *why* logind failed, only on whether
/// it did, and flattening `zbus::Error` here keeps the trait free of a
/// D-Bus-shaped dependency the fakes would otherwise have to fabricate.
#[derive(Debug)]
pub struct LogindError(String);

impl LogindError {
    pub fn new(message: impl Into<String>) -> Self {
        LogindError(message.into())
    }
}

impl fmt::Display for LogindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LogindError {}

/// A locker that could not be started at all (missing binary, bad argv,
/// fork/exec failure). Distinct from "started and then died", which this
/// daemon deliberately never observes — see [`LockerSpawner`].
#[derive(Debug)]
pub struct SpawnError(String);

impl SpawnError {
    pub fn new(message: impl Into<String>) -> Self {
        SpawnError(message.into())
    }
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SpawnError {}

/// The two things this daemon needs from logind. Deliberately tiny: the
/// signal *streams* are not on the trait (they are plumbing that [`run`]
/// owns and feeds to the machine as [`SleepEvent`]s), so a fake only has to
/// answer two questions.
pub trait Logind: Send + Sync + 'static {
    /// `Inhibit("sleep", "saola-session", …, "delay") -> fd`
    /// (`docs/SIGNALS.md` §3). The returned [`Inhibitor`] holds the fd; the
    /// inhibitor is released by dropping it.
    fn acquire_delay_inhibitor(&self) -> BoxFuture<'_, Result<Inhibitor, LogindError>>;

    /// Reads the session's `LockedHint` property — the one and only
    /// definition of "the session is locked" in this crate (Stage 2's
    /// decision section).
    fn locked_hint(&self) -> BoxFuture<'_, Result<bool, LogindError>>;
}

/// Starting the locker. One method, and it returns as soon as the child
/// exists: the daemon never waits for the locker to exit and never reads its
/// output (Architecture's spawn hygiene). "Success" here means fork/exec
/// succeeded, nothing more — whether the lock actually took effect is
/// answered by [`Logind::locked_hint`], never by the child process.
pub trait LockerSpawner: Send + Sync + 'static {
    fn spawn_locker(&self) -> BoxFuture<'_, Result<(), SpawnError>>;
}

/// Why we are locking. Carried only into log lines, so a `journalctl` reader
/// can tell an idle timeout from a lid close from `loginctl lock-session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockTrigger {
    /// `PrepareForSleep(true)` — the machine is about to suspend.
    BeforeSleep,
    /// logind's session `Lock` signal, i.e. `loginctl lock-session`
    /// (`docs/SIGNALS.md` §3: niri does not consume this signal, so this
    /// daemon is what makes that command do anything at all).
    LogindLockSignal,
    /// Stage 5's `ext-idle-notify-v1` lock timeout. Constructed by
    /// `idle.rs`, not by this module — the point of it being here is that
    /// the idle module reuses [`SessionLocker::lock_if_needed`] rather than
    /// growing a second spawn path.
    #[allow(dead_code)]
    IdleTimeout,
}

impl fmt::Display for LockTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockTrigger::BeforeSleep => f.write_str("before-sleep"),
            LockTrigger::LogindLockSignal => f.write_str("logind Lock signal"),
            LockTrigger::IdleTimeout => f.write_str("idle timeout"),
        }
    }
}

/// What one spawn-if-not-locked attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockAttempt {
    /// `LockedHint` was already `true` — no locker spawned, because
    /// Architecture is explicit: "never stack lockers".
    AlreadyLocked,
    /// A locker was spawned by (possibly) a different clone of this
    /// `SessionLocker` less than [`LOCK_SETTLE_GRACE`] ago — `LockedHint`
    /// may not have caught up yet (Stage 7's finding E-3), so we treat this
    /// exactly like `AlreadyLocked` for the purposes of *spawning*, but the
    /// before-sleep path still waits out confirmation on it (see
    /// `SleepMachine::on_going_to_sleep`), so an actually-stalled locker is
    /// not hidden by this shortcut.
    SpawnPending,
    /// A locker process was started. Says nothing yet about whether its
    /// surfaces are up; that is what confirmation is for.
    Spawned,
    /// fork/exec failed. Logged at error level by the caller; the sleep path
    /// still proceeds (severity rule 2 — and logind would force it anyway).
    SpawnFailed,
}

/// The spawn-if-not-locked path, shared by every trigger.
///
/// **This is the single locker-spawn implementation in the crate.** The
/// before-sleep path, the `Lock`-signal path and (Stage 5) the idle-lock
/// timeout all go through [`SessionLocker::lock_if_needed`]; Architecture
/// requires "one implementation, not two". It is `Clone` (two `Arc`s) so
/// `main.rs` can hand a copy to the idle module.
#[derive(Clone)]
pub struct SessionLocker {
    logind: Arc<dyn Logind>,
    spawner: Arc<dyn LockerSpawner>,
    /// When we last successfully spawned a locker. Shared across every
    /// clone of this `SessionLocker` (the idle module's task and this
    /// module's task each hold one — Stage 7's finding E-3): `LockedHint`
    /// takes up to `LOCK_SETTLE_GRACE` to catch up after a spawn, and two
    /// independent tasks each reading it as `false` inside that window
    /// would both conclude they need to spawn. A `tokio::sync::Mutex`
    /// rather than `std::sync::Mutex`: it must stay held across the whole
    /// read-`LockedHint`-then-spawn sequence below (both `.await` points),
    /// which is what actually serialises the two callers — a plain
    /// check-then-set of just the timestamp would still let both callers
    /// read a stale `LockedHint` before either had spawned.
    last_spawn: Arc<tokio::sync::Mutex<Option<Instant>>>,
}

impl fmt::Debug for SessionLocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionLocker")
    }
}

impl SessionLocker {
    pub fn new(logind: Arc<dyn Logind>, spawner: Arc<dyn LockerSpawner>) -> Self {
        SessionLocker {
            logind,
            spawner,
            last_spawn: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Spawns the locker unless the session is already locked (or was very
    /// recently spawned by another clone of this `SessionLocker` — see
    /// [`LOCK_SETTLE_GRACE`] and Stage 7's finding E-3).
    ///
    /// Note the failure bias, which is Architecture's severity order applied
    /// directly: if `LockedHint` cannot be *read*, we assume **unlocked** and
    /// spawn. That risks a spurious lock (severity 3) to avoid the
    /// possibility of sleeping or idling with the session exposed (severity
    /// 1). Never the other way round.
    pub async fn lock_if_needed(&self, trigger: LockTrigger) -> LockAttempt {
        // Held for the whole sequence below — see the field doc comment for
        // why holding it only around the timestamp would not be enough.
        let mut last_spawn = self.last_spawn.lock().await;

        let already_locked = match self.logind.locked_hint().await {
            Ok(locked) => locked,
            Err(err) => {
                warn!(
                    %trigger,
                    error = %err,
                    "saola-session: could not read logind LockedHint; assuming the session is \
                     UNLOCKED and spawning the locker (a spurious lock is preferable to an \
                     exposed session)"
                );
                false
            }
        };

        if already_locked {
            info!(
                %trigger,
                "saola-session: session already locked (LockedHint=true) — not spawning a second locker"
            );
            return LockAttempt::AlreadyLocked;
        }

        // `LockedHint` says unlocked — but it might just be lagging behind a
        // spawn a *different* clone of this `SessionLocker` made moments
        // ago (E-3). Only consulted once we know `LockedHint` itself is not
        // already the more authoritative "true".
        if let Some(spawned_at) = *last_spawn {
            let elapsed = Instant::now().saturating_duration_since(spawned_at);
            if elapsed < LOCK_SETTLE_GRACE {
                info!(
                    %trigger,
                    elapsed_ms = elapsed.as_millis(),
                    "saola-session: a locker was spawned recently and LockedHint may not have \
                     caught up yet — treating the session as already locked rather than risk \
                     stacking a second locker"
                );
                return LockAttempt::SpawnPending;
            }
        }

        match self.spawner.spawn_locker().await {
            Ok(()) => {
                *last_spawn = Some(Instant::now());
                info!(%trigger, "saola-session: locker spawned");
                LockAttempt::Spawned
            }
            Err(err) => {
                error!(
                    %trigger,
                    error = %err,
                    "saola-session: FAILED to spawn the locker — the session is NOT being locked"
                );
                LockAttempt::SpawnFailed
            }
        }
    }
}

/// The three states of Architecture's diagram. `LockPending` is transient —
/// it is only observable from inside [`SleepMachine::handle_event`], which
/// holds `&mut self` for the whole sequence — but it is a real stored state
/// so that any future code (and any log line) can tell "we are mid-sequence"
/// from "we are done and the inhibitor is gone".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepState {
    /// Normal running. The inhibitor is held (unless logind refused it, or
    /// `lock-before-sleep #false` means we never asked).
    Awake,
    /// `PrepareForSleep(true)` is being handled: locker spawned (or skipped),
    /// confirmation being awaited.
    LockPending,
    /// The inhibitor has been released; logind is free to suspend. Left only
    /// by `PrepareForSleep(false)` on resume.
    ReadyToSleep,
}

/// How the `LockPending` sequence ended. Every variant releases the
/// inhibitor — that is the point of the deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepReadiness {
    /// The session was already locked; nothing to spawn, nothing to wait for.
    AlreadyLocked,
    /// The locker was spawned and `LockedHint` flipped to `true` inside the
    /// deadline. The happy path.
    Confirmed,
    /// The locker was spawned but confirmation never arrived before the
    /// deadline. We release anyway (severity rule 2, and logind's
    /// `InhibitDelayMaxUSec` would force it in another few seconds
    /// regardless) and log loudly.
    Unconfirmed,
    /// The locker could not be started at all. Release immediately: there is
    /// no process that could ever confirm, and holding the inhibitor cannot
    /// conjure one.
    SpawnFailed,
}

/// One thing that happened, fed to the machine. [`run`] translates logind's
/// D-Bus signals into these; tests construct them directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepEvent {
    /// logind's `PrepareForSleep(b)`: `true` right before sleep, `false`
    /// right after resume (`docs/SIGNALS.md` §3).
    PrepareForSleep(bool),
    /// The session's `Lock` signal (`loginctl lock-session`).
    Lock,
    /// The session's `Unlock` signal. See [`SleepMachine::handle_event`] for
    /// why this deliberately does nothing.
    Unlock,
    /// `org.freedesktop.DBus`'s `NameOwnerChanged` fired for
    /// `org.freedesktop.login1` — logind itself restarted underneath us
    /// (Stage 7's finding E-2). Any inhibitor fd we hold refers to the old
    /// logind process and is now meaningless; `PrepareForSleep`/`Lock`/
    /// `Unlock` do *not* end when this happens (their match rules key on the
    /// well-known name and silently re-resolve to whoever owns it now), so
    /// this is the one signal that actually notices the restart.
    LogindRestarted,
}

/// What the machine did with one event. Returned rather than only logged so
/// the tests can assert on behavior instead of scraping log output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutcome {
    /// Nothing to do: a duplicate `PrepareForSleep(true)`, an `Unlock`, or an
    /// event that `lock-before-sleep #false` disables.
    NoAction,
    /// The `LockPending` sequence ran to completion (and released).
    PreparedForSleep(SleepReadiness),
    /// A spawn-if-not-locked attempt ran (the `Lock` signal path).
    Locked(LockAttempt),
    /// Resume handled. `inhibitor_held` is false only if logind refused to
    /// give the inhibitor back, which is logged at error level.
    Resumed { inhibitor_held: bool },
    /// A `LogindRestarted` event was handled: the possibly-stale inhibitor
    /// was released and a fresh one requested. `inhibitor_held` is false
    /// only if the fresh acquisition also failed (E-2).
    LogindRestarted { inhibitor_held: bool },
}

/// Architecture's sleep state machine. Owns the inhibitor; drives the two
/// traits; knows nothing about D-Bus.
pub struct SleepMachine {
    logind: Arc<dyn Logind>,
    locker: SessionLocker,
    /// `lock-before-sleep` from `session.kdl`. When false we never take an
    /// inhibitor and never act on `PrepareForSleep` — but we still honor the
    /// `Lock` signal, which is a separate concern the operator did not turn
    /// off.
    lock_before_sleep: bool,
    /// The `LockPending` hard deadline — see [`confirmation_deadline`].
    deadline: Duration,
    state: SleepState,
    /// `Some` while we are delaying sleep. Dropping it is the release.
    inhibitor: Option<Inhibitor>,
}

impl SleepMachine {
    pub fn new(
        logind: Arc<dyn Logind>,
        locker: SessionLocker,
        lock_before_sleep: bool,
        deadline: Duration,
    ) -> Self {
        SleepMachine {
            logind,
            locker,
            lock_before_sleep,
            deadline,
            state: SleepState::Awake,
            inhibitor: None,
        }
    }

    pub fn state(&self) -> SleepState {
        self.state
    }

    pub fn holds_inhibitor(&self) -> bool {
        self.inhibitor.is_some()
    }

    /// Updates the `LockPending` deadline in place. Used by [`run`]'s E-1
    /// retry loop: a daemon that started degraded uses
    /// [`DEFAULT_CONFIRMATION_DEADLINE`] as a placeholder (there was no
    /// connection to ask `InhibitDelayMaxUSec` of), and this lets a later
    /// successful reconnect replace it with the real, machine-specific
    /// deadline instead of staying pinned to the placeholder forever.
    pub fn set_deadline(&mut self, deadline: Duration) {
        self.deadline = deadline;
    }

    /// Startup: take the delay inhibitor (Architecture: "taken at startup").
    ///
    /// Called by [`run`] *after* the signal subscriptions are in place — see
    /// [`connect`] for why that ordering is the safe one.
    pub async fn start(&mut self) {
        if !self.lock_before_sleep {
            info!(
                "saola-session: lock-before-sleep is disabled in session.kdl — not taking a \
                 logind delay inhibitor; the logind Lock signal is still honored"
            );
            return;
        }
        self.ensure_inhibitor().await;
    }

    /// Shutdown: drop the inhibitor explicitly so the release is logged
    /// rather than happening silently in a destructor somewhere.
    pub fn release_for_shutdown(&mut self) {
        self.release_inhibitor("daemon shutting down");
    }

    /// Feeds one event through the machine.
    pub async fn handle_event(&mut self, event: SleepEvent) -> EventOutcome {
        match event {
            SleepEvent::PrepareForSleep(true) => self.on_going_to_sleep().await,
            SleepEvent::PrepareForSleep(false) => self.on_resume().await,
            SleepEvent::Lock => {
                let attempt = self
                    .locker
                    .lock_if_needed(LockTrigger::LogindLockSignal)
                    .await;
                EventOutcome::Locked(attempt)
            }
            SleepEvent::Unlock => {
                // Deliberately inert, and this is a security decision rather
                // than an omission. `Unlock` is a momentary "someone asked
                // for the session to be unlocked" event (`docs/SIGNALS.md`
                // §3) — it carries no authentication. The only thing this
                // daemon could do with it is kill the locker process, which
                // would turn `loginctl unlock-session` into an
                // authentication bypass for anyone on the session bus.
                // Unlocking belongs to the locker, behind PAM, and nowhere
                // else. We subscribe purely so the journal shows the request
                // arrived.
                info!(
                    "saola-session: logind Unlock signal received — ignored by design (unlocking \
                     is the locker's job, behind authentication)"
                );
                EventOutcome::NoAction
            }
            SleepEvent::LogindRestarted => {
                if !self.lock_before_sleep {
                    debug!(
                        "saola-session: logind restarted with lock-before-sleep disabled — \
                         nothing to re-acquire"
                    );
                    return EventOutcome::NoAction;
                }
                warn!(
                    "saola-session: systemd-logind restarted — any sleep inhibitor fd we held \
                     refers to a logind that no longer exists; releasing it and acquiring a \
                     fresh one against the new logind"
                );
                // `release_inhibitor` before `ensure_inhibitor` is what
                // defeats `ensure_inhibitor`'s `is_some()` short-circuit —
                // holding a stale fd is worth nothing anyway, so releasing
                // first is safe even though we are about to ask again
                // immediately.
                self.release_inhibitor("logind restarted — the held fd (if any) is stale");
                let inhibitor_held = self.ensure_inhibitor().await;
                EventOutcome::LogindRestarted { inhibitor_held }
            }
        }
    }

    /// `PrepareForSleep(true)` — the `LockPending` sequence.
    async fn on_going_to_sleep(&mut self) -> EventOutcome {
        if !self.lock_before_sleep {
            debug!(
                "saola-session: PrepareForSleep(true) with lock-before-sleep disabled — nothing to do"
            );
            return EventOutcome::NoAction;
        }

        if self.state == SleepState::ReadyToSleep {
            // logind restarts, and a suspend that gets aborted and retried,
            // can both produce a second `true` without an intervening
            // `false`. Spawning again would stack lockers on top of a lock
            // that is already up; releasing again would be a no-op. So:
            // nothing, loudly enough to notice in the journal.
            warn!(
                "saola-session: duplicate PrepareForSleep(true) while already ready to sleep — \
                 ignoring (not spawning a second locker)"
            );
            return EventOutcome::NoAction;
        }

        self.state = SleepState::LockPending;

        let readiness = match self.locker.lock_if_needed(LockTrigger::BeforeSleep).await {
            LockAttempt::AlreadyLocked => SleepReadiness::AlreadyLocked,
            LockAttempt::SpawnFailed => {
                // Already logged at error level by `lock_if_needed`. Nothing
                // will ever confirm, so waiting out the deadline would only
                // delay the suspend for nothing (severity rule 2).
                SleepReadiness::SpawnFailed
            }
            // `SpawnPending` (E-3) means *some* clone of this `SessionLocker`
            // spawned a locker within the last `LOCK_SETTLE_GRACE` — we did
            // not spawn a second one, but we still wait out confirmation on
            // it exactly as if we had, so a locker that spawned but then
            // genuinely stalled still produces the loud `Unconfirmed`
            // warning rather than being silently treated as done.
            LockAttempt::Spawned | LockAttempt::SpawnPending => {
                match self.await_lock_confirmation().await {
                    Some(elapsed) => {
                        info!(
                            confirmation_ms = elapsed.as_millis(),
                            "saola-session: lock confirmed (LockedHint=true) before sleep"
                        );
                        SleepReadiness::Confirmed
                    }
                    None => {
                        warn!(
                            deadline_ms = self.deadline.as_millis(),
                            "saola-session: lock NOT confirmed within the deadline — releasing the \
                         sleep inhibitor and letting the suspend proceed; the locker was started, \
                         so it should be on screen by resume"
                        );
                        SleepReadiness::Unconfirmed
                    }
                }
            }
        };

        // Every path releases. This is the whole reason the deadline exists.
        self.release_inhibitor("sleep may proceed");
        self.state = SleepState::ReadyToSleep;
        EventOutcome::PreparedForSleep(readiness)
    }

    /// `PrepareForSleep(false)` — resume.
    ///
    /// Architecture is emphatic: the inhibitor is "re-acquired on every
    /// resume **before anything else** — a daemon that sleeps without
    /// holding it has silently become decorative". So the very first thing
    /// this does is ask for it back; state, logging and everything else come
    /// after.
    async fn on_resume(&mut self) -> EventOutcome {
        if !self.lock_before_sleep {
            self.state = SleepState::Awake;
            debug!(
                "saola-session: PrepareForSleep(false) with lock-before-sleep disabled — nothing to do"
            );
            return EventOutcome::NoAction;
        }

        let inhibitor_held = self.ensure_inhibitor().await;
        self.state = SleepState::Awake;
        info!(
            inhibitor_held,
            "saola-session: resumed from sleep (PrepareForSleep(false))"
        );
        EventOutcome::Resumed { inhibitor_held }
    }

    /// Makes sure we hold an inhibitor, retrying a bounded number of times.
    /// Returns whether we ended up holding one.
    ///
    /// Failing to hold it is *not* fatal here: dying would leave the machine
    /// with no locker spawner at all, which is strictly worse than running
    /// without the delay (we would still spawn the locker on the next
    /// `PrepareForSleep`, just without winning the race for certain). It is
    /// logged at error level with the consequence spelled out, and it is
    /// flagged in Stage 4's handoff as something Stage 7's exposure audit
    /// should look at again.
    async fn ensure_inhibitor(&mut self) -> bool {
        if self.inhibitor.is_some() {
            debug!("saola-session: sleep inhibitor already held; nothing to re-acquire");
            return true;
        }

        for attempt in 1..=INHIBITOR_ACQUIRE_ATTEMPTS {
            match self.logind.acquire_delay_inhibitor().await {
                Ok(inhibitor) => {
                    self.inhibitor = Some(inhibitor);
                    info!(
                        what = INHIBIT_WHAT,
                        who = INHIBIT_WHO,
                        why = INHIBIT_WHY,
                        mode = INHIBIT_MODE,
                        "saola-session: holding a logind delay inhibitor"
                    );
                    return true;
                }
                Err(err) => {
                    warn!(
                        attempt,
                        attempts = INHIBITOR_ACQUIRE_ATTEMPTS,
                        error = %err,
                        "saola-session: could not take the logind sleep inhibitor"
                    );
                    if attempt < INHIBITOR_ACQUIRE_ATTEMPTS {
                        tokio::time::sleep(INHIBITOR_RETRY_BACKOFF).await;
                    }
                }
            }
        }

        error!(
            attempts = INHIBITOR_ACQUIRE_ATTEMPTS,
            "saola-session: NO sleep inhibitor held — the next suspend will not be delayed for \
             the locker, so the session may reach sleep before the lock surface is up"
        );
        false
    }

    /// Drops the inhibitor (which *is* the release) and says so.
    fn release_inhibitor(&mut self, reason: &str) {
        match self.inhibitor.take() {
            Some(inhibitor) => {
                // Explicit, so the release reads as an action rather than as
                // a value quietly going out of scope.
                drop(inhibitor);
                info!(reason, "saola-session: released the logind sleep inhibitor");
            }
            None => debug!(
                reason,
                "saola-session: no sleep inhibitor held; nothing to release"
            ),
        }
    }

    /// Polls `LockedHint` until it is `true` or the deadline expires.
    /// Returns how long confirmation took, or `None` on deadline.
    async fn await_lock_confirmation(&self) -> Option<Duration> {
        let started = Instant::now();

        // The inner future never completes on its own unless the lock is
        // confirmed; `timeout` is what bounds it. Read-failures do not abort
        // the wait — logind could be briefly unavailable while the lock is
        // coming up — but they are logged once, not once per poll.
        let poll = async {
            let mut warned = false;
            loop {
                match self.logind.locked_hint().await {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(err) => {
                        if !warned {
                            warned = true;
                            warn!(
                                error = %err,
                                "saola-session: could not read LockedHint while waiting for lock \
                                 confirmation; will keep trying until the deadline"
                            );
                        }
                    }
                }
                tokio::time::sleep(CONFIRMATION_POLL_INTERVAL).await;
            }
        };

        match tokio::time::timeout(self.deadline, poll).await {
            Ok(()) => Some(started.elapsed()),
            Err(_) => None,
        }
    }
}

/// Picks the `LockPending` deadline given logind's own cap.
///
/// Never larger than [`DEFAULT_CONFIRMATION_DEADLINE`], and never more than
/// four fifths of `InhibitDelayMaxUSec` — so on a machine configured with a
/// tighter cap than this one's 5 s we still release before logind stops
/// waiting for us, leaving a fifth of the budget as margin for the release
/// itself. Stage 7's finding E-6: this used to be three fifths, which on
/// this machine's 5 s cap computed 3 s and then lost to the (then 2 s)
/// default anyway — the ratio never actually bound here. Four fifths of 5 s
/// is 4 s, matching the new default exactly, so on this machine the two
/// numbers agree instead of one silently overriding the other.
/// `checked_mul` rather than `*`: `Duration` arithmetic panics on overflow,
/// and this input comes off the bus (no-panic rule).
fn confirmation_deadline(inhibit_delay_max: Option<Duration>) -> Duration {
    match inhibit_delay_max {
        None => DEFAULT_CONFIRMATION_DEADLINE,
        Some(max) => max
            .checked_mul(4)
            .map(|budget| budget / 5)
            .unwrap_or(DEFAULT_CONFIRMATION_DEADLINE)
            .min(DEFAULT_CONFIRMATION_DEADLINE),
    }
}

/// Splits `locker "…"` into a program and its arguments.
///
/// Plain whitespace splitting, no shell: no quoting, no globbing, no `$VAR`.
/// Stage 3 stored the knob as an unvalidated command line and left the
/// splitting to this stage; the deliberate limitation is that a locker path
/// containing spaces is not expressible. That is worth it — running the
/// string through a shell would make the config file a code-execution
/// surface for anything that can write it, for no gain over `argv`.
fn split_command(command: &str) -> Option<(String, Vec<String>)> {
    let mut parts = command.split_whitespace().map(str::to_owned);
    let program = parts.next()?;
    Some((program, parts.collect()))
}

/// The real locker spawner: `tokio::process`, detached.
pub struct CommandLocker {
    command: String,
}

impl CommandLocker {
    pub fn new(command: String) -> Self {
        CommandLocker { command }
    }
}

impl LockerSpawner for CommandLocker {
    fn spawn_locker(&self) -> BoxFuture<'_, Result<(), SpawnError>> {
        Box::pin(async move {
            let (program, args) = split_command(&self.command)
                .ok_or_else(|| SpawnError::new("the configured locker command is empty"))?;

            let mut command = tokio::process::Command::new(&program);
            command.args(&args);
            // No stdin: the locker reads its password from Wayland, and a
            // daemon-inherited stdin is at best useless.
            command.stdin(Stdio::null());
            // stdout/stderr stay inherited: under `systemd --user` that is
            // the journal, which is where a locker's own complaints belong.
            // We never read them — Architecture: "never parses its output".

            match command.spawn() {
                Ok(child) => {
                    debug!(
                        program,
                        pid = child.id(),
                        "saola-session: locker process started (detached)"
                    );
                    // Dropping the handle is what "detached" means here. We
                    // deliberately do not `wait()`: the locker outlives this
                    // sequence by design, and its exit status tells us
                    // nothing we act on (confirmation comes from
                    // `LockedHint`, not from the child). Tokio's runtime
                    // reaps dropped children through its own orphan queue on
                    // SIGCHLD, so this does not leak zombies — it is
                    // specifically why `main.rs` builds the runtime with
                    // `enable_all()`.
                    drop(child);
                    Ok(())
                }
                Err(err) => Err(SpawnError::new(format!(
                    "could not start locker '{program}': {err}"
                ))),
            }
        })
    }
}

/// The generated zbus proxies for the three logind interfaces this module
/// touches. Wrapped in a module with `dead_code` allowed because
/// `#[zbus::proxy]` generates more surface than we call (property-change
/// listeners, argument accessors) and `-D warnings` would otherwise reject
/// the crate for code we never wrote.
///
/// Every signature below is copied from `docs/SIGNALS.md` §3 rather than
/// re-derived. Note the two `name = "…PID"` overrides: zbus derives the
/// D-Bus member name by pascal-casing the Rust one, which would produce
/// `GetSessionByPid` — logind's actual member is `GetSessionByPID`.
mod proxies {
    #![allow(dead_code)]

    use zbus::zvariant::{OwnedFd, OwnedObjectPath};

    #[zbus::proxy(
        interface = "org.freedesktop.login1.Manager",
        default_service = "org.freedesktop.login1",
        default_path = "/org/freedesktop/login1",
        gen_blocking = false
    )]
    pub(super) trait LogindManager {
        /// `Inhibit(what, who, why, mode) -> fd` — `ssss -> h`.
        fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

        fn get_session(&self, session_id: &str) -> zbus::Result<OwnedObjectPath>;

        #[zbus(name = "GetSessionByPID")]
        fn get_session_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;

        #[zbus(name = "GetUserByPID")]
        fn get_user_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;

        /// Microseconds. 5,000,000 on Jordan's machine (`docs/SIGNALS.md` §3).
        #[zbus(property, name = "InhibitDelayMaxUSec")]
        fn inhibit_delay_max_usec(&self) -> zbus::Result<u64>;

        /// Emitted **by** logind: `true` right before sleep, `false` right
        /// after resume.
        #[zbus(signal)]
        fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
    }

    #[zbus::proxy(
        interface = "org.freedesktop.login1.Session",
        default_service = "org.freedesktop.login1",
        gen_blocking = false
    )]
    pub(super) trait LogindSession {
        /// Set by niri via `SetLockedHint` — this crate's definition of
        /// "locked" (Stage 2's decision).
        #[zbus(property)]
        fn locked_hint(&self) -> zbus::Result<bool>;

        /// `loginctl lock-session` → `Session.Lock()` → this signal.
        #[zbus(signal)]
        fn lock(&self) -> zbus::Result<()>;

        #[zbus(signal)]
        fn unlock(&self) -> zbus::Result<()>;
    }

    #[zbus::proxy(
        interface = "org.freedesktop.login1.User",
        default_service = "org.freedesktop.login1",
        gen_blocking = false
    )]
    pub(super) trait LogindUser {
        /// `(session_id, object_path)` of the user's display session — the
        /// last-resort way to find our own session when neither
        /// `$XDG_SESSION_ID` nor our PID resolves one (the `systemd --user`
        /// case, where the daemon's own cgroup is outside any session scope).
        #[zbus(property)]
        fn display(&self) -> zbus::Result<(String, OwnedObjectPath)>;
    }
}

/// The real [`Logind`]: two zbus proxies and nothing else.
pub struct LogindConnection {
    manager: proxies::LogindManagerProxy<'static>,
    session: proxies::LogindSessionProxy<'static>,
}

impl Logind for LogindConnection {
    fn acquire_delay_inhibitor(&self) -> BoxFuture<'_, Result<Inhibitor, LogindError>> {
        Box::pin(async move {
            let fd = self
                .manager
                .inhibit(INHIBIT_WHAT, INHIBIT_WHO, INHIBIT_WHY, INHIBIT_MODE)
                .await
                .map_err(|err| LogindError::new(format!("logind Inhibit() failed: {err}")))?;
            // The fd *is* the inhibitor: closing it releases the lock.
            Ok(Inhibitor::new(fd))
        })
    }

    fn locked_hint(&self) -> BoxFuture<'_, Result<bool, LogindError>> {
        Box::pin(async move {
            self.session
                .locked_hint()
                .await
                .map_err(|err| LogindError::new(format!("reading logind LockedHint failed: {err}")))
        })
    }
}

/// The four logind-related signal streams [`run`]'s event loop selects over.
struct LogindSignals {
    prepare_for_sleep: proxies::PrepareForSleepStream,
    lock: proxies::LockStream,
    unlock: proxies::UnlockStream,
    /// `org.freedesktop.DBus`'s `NameOwnerChanged`, filtered server-side to
    /// `arg0='org.freedesktop.login1'` — Stage 7's finding E-2's mechanism
    /// for noticing a `systemd-logind` restart, which invalidates any
    /// inhibitor fd we hold without ending this stream or any of the other
    /// three (all three ride a well-known-name match rule that silently
    /// re-resolves to whichever process owns `org.freedesktop.login1`
    /// right now, so a restart is invisible to them by construction).
    logind_owner_changed: zbus::fdo::NameOwnerChangedStream,
}

/// Everything `main.rs` needs to start this module, plus the handle Stage 5
/// borrows. Built by [`connect`], consumed by [`run`].
pub struct SleepWiring {
    locker: SessionLocker,
    /// The concrete, swappable handle — kept here (rather than only the
    /// `Arc<dyn Logind>` view `SleepMachine`/`SessionLocker` hold) so
    /// [`run`]'s retry loop can call [`SwappableLogind::replace`] on it.
    logind: Arc<SwappableLogind>,
    /// `None` means logind was unreachable at `connect` time — see
    /// [`connect`]'s degraded mode and [`run`]'s E-1 retry loop, which no
    /// longer treats this as permanent.
    signals: Option<LogindSignals>,
    lock_before_sleep: bool,
    deadline: Duration,
}

impl SleepWiring {
    /// The shared spawn-if-not-locked path, for the idle module (Stage 5) to
    /// call on its lock timeout. Cloning it is two `Arc` bumps.
    pub fn session_locker(&self) -> SessionLocker {
        self.locker.clone()
    }
}

/// A [`Logind`] for when there is no logind: every call fails, loudly enough
/// once (at connect time) and quietly thereafter.
///
/// This exists for the nested-niri case `CLAUDE.md` makes binding: a nested
/// compositor is not a logind session, and `sleep.rs` "must log-and-continue
/// when its logind side is degraded in a nested test, never crash". Because
/// [`SessionLocker::lock_if_needed`] treats an unreadable `LockedHint` as
/// "unlocked", Stage 5's idle-lock still spawns the locker in that
/// environment — degraded, but not broken.
struct UnavailableLogind;

impl Logind for UnavailableLogind {
    fn acquire_delay_inhibitor(&self) -> BoxFuture<'_, Result<Inhibitor, LogindError>> {
        Box::pin(async { Err(LogindError::new("logind is not available in this session")) })
    }

    fn locked_hint(&self) -> BoxFuture<'_, Result<bool, LogindError>> {
        Box::pin(async { Err(LogindError::new("logind is not available in this session")) })
    }
}

/// A [`Logind`] whose underlying implementation can be replaced after
/// construction — Stage 7's finding E-1's mechanism for making a *retried*
/// reconnect actually reach every holder of the trait object, not just
/// [`SleepMachine`]'s own copy.
///
/// [`SessionLocker`] (shared between this module and `idle.rs`'s task via
/// `SessionLocker::clone`) and [`SleepMachine`] are both built, once, around
/// the *same* `Arc<dyn Logind>` that [`connect`] constructs. If each had
/// instead captured its own concrete `Arc<LogindConnection>` at connect
/// time, a reconnect inside [`run`]'s retry loop could only ever fix
/// `SleepMachine`'s copy — `SessionLocker`'s `LockedHint` checks (and
/// idle.rs's clone of it) would stay pointed at the connection that failed
/// at startup, forever. Routing every call through one
/// `RwLock<Arc<dyn Logind>>` means [`SwappableLogind::replace`] updates both
/// at once, from the one place ([`run`]) that discovers a reconnect
/// succeeded — without threading a callback or a second channel through
/// either type.
///
/// Teaching note: this is the same "hold the trait object behind
/// `Arc<dyn Trait>` so tests and callers can't tell a fake from the real
/// thing" trick the module doc comment already explains, one layer up —
/// `SwappableLogind` itself implements `Logind`, so `SleepMachine` and
/// `SessionLocker` never need to know their `Logind` might be swapped out
/// from under them mid-call; the `RwLock` only ever blocks a caller for the
/// instant a `replace` is in flight, which happens at most once every
/// [`RECONNECT_INTERVAL`].
struct SwappableLogind {
    current: tokio::sync::RwLock<Arc<dyn Logind>>,
}

impl SwappableLogind {
    fn new(initial: Arc<dyn Logind>) -> Self {
        SwappableLogind {
            current: tokio::sync::RwLock::new(initial),
        }
    }

    /// Atomically points every holder of this `SwappableLogind` at a new
    /// underlying connection.
    async fn replace(&self, new: Arc<dyn Logind>) {
        *self.current.write().await = new;
    }
}

impl Logind for SwappableLogind {
    fn acquire_delay_inhibitor(&self) -> BoxFuture<'_, Result<Inhibitor, LogindError>> {
        Box::pin(async move {
            let current = self.current.read().await.clone();
            current.acquire_delay_inhibitor().await
        })
    }

    fn locked_hint(&self) -> BoxFuture<'_, Result<bool, LogindError>> {
        Box::pin(async move {
            let current = self.current.read().await.clone();
            current.locked_hint().await
        })
    }
}

/// Connects to logind and subscribes to its signals, or returns a degraded
/// wiring that still spawns lockers but cannot delay sleep.
///
/// **Ordering matters, and it is subscribe-then-inhibit.** Between the two
/// there is a window in which a suspend could start. If we took the
/// inhibitor first and a suspend began before we subscribed, we would hold a
/// delay nobody ever releases *and* miss the `PrepareForSleep` that would
/// have made us lock — five seconds of nothing, then an unlocked suspend
/// (severity 1). Subscribing first means a suspend in the window still
/// reaches us and still spawns the locker; we merely lose the delay we never
/// held. The first ordering trades a certainty for a worse certainty; this
/// one trades it for a risk.
pub async fn connect(config: &SessionConfig) -> SleepWiring {
    let spawner: Arc<dyn LockerSpawner> = Arc::new(CommandLocker::new(config.locker.clone()));

    let (initial, signals, deadline): (Arc<dyn Logind>, Option<LogindSignals>, Duration) =
        match connect_logind().await {
            Ok((connection, signals, inhibit_delay_max)) => {
                let deadline = confirmation_deadline(inhibit_delay_max);
                info!(
                    inhibit_delay_max_ms = inhibit_delay_max.map(|d| d.as_millis()),
                    confirmation_deadline_ms = deadline.as_millis(),
                    "saola-session: connected to logind"
                );
                (Arc::new(connection), Some(signals), deadline)
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "saola-session: logind is unavailable at startup — before-sleep locking and \
                     `loginctl lock-session` are inert until a reconnect succeeds (retried every \
                     {RECONNECT_INTERVAL:?} — Stage 7's finding E-1; expected inside a nested \
                     niri, which is not a logind session and will retry harmlessly forever; \
                     self-healing in a real session once logind becomes reachable)"
                );
                (
                    Arc::new(UnavailableLogind),
                    None,
                    DEFAULT_CONFIRMATION_DEADLINE,
                )
            }
        };

    // See `SwappableLogind`'s doc comment for why `SessionLocker` and
    // `SleepMachine` (built later, in `run`) both need to be handed the
    // *same* swappable handle rather than each capturing their own
    // `initial` directly.
    let swappable = Arc::new(SwappableLogind::new(initial));
    // Method-call syntax (not `Arc::clone(&swappable)`): resolving `T` from
    // the receiver's own type first, rather than from the `let` binding's
    // expected type, is what lets the unsizing coercion to `Arc<dyn Logind>`
    // apply cleanly on the result.
    let logind: Arc<dyn Logind> = swappable.clone();

    SleepWiring {
        locker: SessionLocker::new(logind, spawner),
        logind: swappable,
        signals,
        lock_before_sleep: config.lock_before_sleep,
        deadline,
    }
}

/// The zbus half of [`connect`]. Split out so the error path there stays one
/// `match` rather than five.
async fn connect_logind() -> Result<(LogindConnection, LogindSignals, Option<Duration>), LogindError>
{
    let connection = zbus::Connection::system()
        .await
        .map_err(|err| LogindError::new(format!("no system bus connection: {err}")))?;

    // `CacheProperties::No`: every `locked_hint()` becomes a real `Get` round
    // trip. zbus's default property cache is fed by `PropertiesChanged`, and
    // a cache that misses one edge would mean this daemon believes the
    // session is locked when it is not — the one belief that must never be
    // wrong here (severity 1). A D-Bus round trip every 50 ms for at most two
    // seconds is a cheap price for the property never being stale.
    let manager = proxies::LogindManagerProxy::builder(&connection)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|err| LogindError::new(format!("logind Manager proxy failed: {err}")))?;

    let session_path = resolve_session_path(&connection, &manager).await?;
    info!(session = %session_path.as_str(), "saola-session: resolved our logind session");

    let session = proxies::LogindSessionProxy::builder(&connection)
        .path(session_path)
        .map_err(|err| LogindError::new(format!("bad logind session path: {err}")))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|err| LogindError::new(format!("logind Session proxy failed: {err}")))?;

    // Subscribe before anyone takes an inhibitor — see `connect`'s doc.
    let prepare_for_sleep = manager
        .receive_prepare_for_sleep()
        .await
        .map_err(|err| LogindError::new(format!("PrepareForSleep subscription failed: {err}")))?;
    let lock = session
        .receive_lock()
        .await
        .map_err(|err| LogindError::new(format!("session Lock subscription failed: {err}")))?;
    let unlock = session
        .receive_unlock()
        .await
        .map_err(|err| LogindError::new(format!("session Unlock subscription failed: {err}")))?;

    // E-2: subscribe to the bus daemon's own `NameOwnerChanged` so a
    // `systemd-logind` restart is observable at all — see `LogindSignals`'s
    // doc comment for why the three subscriptions above cannot notice it
    // themselves. Filtered server-side to arg0, the same
    // `receive_*_with_args` shape `inhibit.rs`'s peer-vanish watcher already
    // uses, so this is not a new pattern in the tree.
    let dbus = zbus::fdo::DBusProxy::new(&connection)
        .await
        .map_err(|err| LogindError::new(format!("system D-Bus proxy failed: {err}")))?;
    let logind_owner_changed = dbus
        .receive_name_owner_changed_with_args(&[(0, "org.freedesktop.login1")])
        .await
        .map_err(|err| {
            LogindError::new(format!(
                "logind NameOwnerChanged subscription failed: {err}"
            ))
        })?;

    // Not fatal if unreadable: `confirmation_deadline(None)` falls back to
    // the built-in 2 s, which is under every default cap we know of.
    let inhibit_delay_max = match manager.inhibit_delay_max_usec().await {
        Ok(usec) => Some(Duration::from_micros(usec)),
        Err(err) => {
            warn!(
                error = %err,
                "saola-session: could not read InhibitDelayMaxUSec; using the built-in deadline"
            );
            None
        }
    };

    Ok((
        LogindConnection { manager, session },
        LogindSignals {
            prepare_for_sleep,
            lock,
            unlock,
            logind_owner_changed,
        },
        inhibit_delay_max,
    ))
}

/// Finds *our* session's object path, three ways, in order of directness.
///
/// It must be the session-specific path: `man org.freedesktop.login1` is
/// explicit that `/session/self` and `/session/auto` **never emit signals**
/// (`docs/SIGNALS.md` §3), so a daemon that subscribed there would silently
/// never see `Lock`.
async fn resolve_session_path(
    connection: &zbus::Connection,
    manager: &proxies::LogindManagerProxy<'_>,
) -> Result<zbus::zvariant::OwnedObjectPath, LogindError> {
    // 1. `$XDG_SESSION_ID` — set for anything started from the session
    //    leader, which is how `cargo run` from a terminal will hit this.
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
        && let Ok(path) = manager.get_session(&id).await
    {
        debug!(id, "saola-session: session resolved from $XDG_SESSION_ID");
        return Ok(path);
    }

    // 2. Our own PID. Works when the daemon is inside a session scope.
    let pid = std::process::id();
    if let Ok(path) = manager.get_session_by_pid(pid).await {
        debug!(pid, "saola-session: session resolved from our PID");
        return Ok(path);
    }

    // 3. The user's *display* session. This is the `systemd --user` case
    //    (Stage 8's unit): the daemon lives in `user@.service`, outside any
    //    session scope, so neither of the above resolves — but the logind
    //    User object still points at the graphical session.
    let user_path = manager
        .get_user_by_pid(pid)
        .await
        .map_err(|err| LogindError::new(format!("GetUserByPID failed: {err}")))?;
    let user = proxies::LogindUserProxy::builder(connection)
        .path(user_path)
        .map_err(|err| LogindError::new(format!("bad logind user path: {err}")))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|err| LogindError::new(format!("logind User proxy failed: {err}")))?;
    let (id, path) = user.display().await.map_err(|err| {
        LogindError::new(format!("reading the user's Display session failed: {err}"))
    })?;
    if path.as_str() == "/" {
        return Err(LogindError::new(
            "the logind user has no display session (is this a graphical login?)",
        ));
    }
    debug!(
        id,
        "saola-session: session resolved from the user's display session"
    );
    Ok(path)
}

/// Polls one signal stream for its next item.
///
/// Teaching note: this is all `futures::StreamExt::next` is — a future that
/// polls the stream once per wake and resolves on the first `Ready`. Writing
/// the four lines by hand keeps `futures-util` out of the dependency tree
/// for one method. It is cancel-safe, which is what `tokio::select!`
/// requires: if another branch wins, dropping this future loses nothing,
/// because all the state lives in the stream, not in the future.
async fn next_signal<S>(stream: &mut S) -> Option<S::Item>
where
    S: zbus::export::futures_core::Stream + Unpin,
{
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

/// The module's task: owns the machine, translates logind's signals into
/// [`SleepEvent`]s, and returns (dropping — releasing — the inhibitor) when
/// `main.rs` says to stop.
///
/// # Degraded mode is a retry loop, not a latch (Stage 7's finding E-1)
///
/// The original Stage 4 shape parked forever the first time `connect_logind`
/// failed, which meant a transient D-Bus hiccup at boot could leave the
/// daemon reporting `active (running)` to systemd while never holding an
/// inhibitor or seeing `PrepareForSleep` again for its entire uptime — the
/// textbook "looks alive, is not" failure `CLAUDE.md` calls worse than a
/// crash. The loop below instead retries `connect_logind()` every
/// [`RECONNECT_INTERVAL`] until it succeeds or shutdown is requested, and
/// [`SwappableLogind::replace`] (see its doc comment) is what lets a
/// mid-flight reconnect actually take effect for every holder of the
/// `Logind` trait object, not just this function's own `machine`.
pub async fn run(wiring: SleepWiring, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let SleepWiring {
        locker,
        logind,
        mut signals,
        lock_before_sleep,
        deadline,
    } = wiring;

    let logind_dyn: Arc<dyn Logind> = logind.clone();
    let mut machine = SleepMachine::new(logind_dyn, locker, lock_before_sleep, deadline);

    // Retry loop: stays here for as long as `signals` is `None`. On the
    // happy path (logind reachable at `connect` time) this runs zero
    // iterations. `Option::take` (not a direct match on `signals`) is what
    // lets this loop body reassign `signals` on both the success and
    // failure arms without the borrow checker seeing a use of a
    // partially-moved local.
    let LogindSignals {
        mut prepare_for_sleep,
        mut lock,
        mut unlock,
        mut logind_owner_changed,
    } = loop {
        if let Some(signals) = signals.take() {
            break signals;
        }

        info!(
            retry_in = ?RECONNECT_INTERVAL,
            "saola-session: sleep module running degraded (no logind) — before-sleep locking and \
             `loginctl lock-session` are inert until a reconnect succeeds"
        );
        tokio::select! {
            // Same reasoning as the main loop below: shutdown must win a
            // simultaneous wakeup, so a stop request is never delayed
            // behind a doomed reconnect attempt.
            biased;

            _ = shutdown.changed() => {
                info!("saola-session: sleep module stopping while degraded");
                return;
            }

            _ = tokio::time::sleep(RECONNECT_INTERVAL) => {
                match connect_logind().await {
                    Ok((connection, new_signals, inhibit_delay_max)) => {
                        info!(
                            "saola-session: logind reconnected — before-sleep locking is live again"
                        );
                        logind.replace(Arc::new(connection)).await;
                        machine.set_deadline(confirmation_deadline(inhibit_delay_max));
                        signals = Some(new_signals);
                    }
                    Err(err) => {
                        warn!(
                            error = %err,
                            "saola-session: still no logind — will retry in {RECONNECT_INTERVAL:?}"
                        );
                    }
                }
            }
        }
    };

    machine.start().await;

    // E-5, folded into E-1's timer per Stage 7's review: periodically retry
    // acquiring the inhibitor if we are connected but somehow not holding
    // one (`ensure_inhibitor`'s own 3-attempt/750ms budget at startup or
    // resume can still exhaust itself under a longer D-Bus outage). The
    // `if` guard on the `select!` arm below means this timer's ticks are
    // simply not polled — and so never fire spuriously into the journal —
    // whenever we already hold an inhibitor, which is the common case.
    let mut retry_tick = tokio::time::interval(RECONNECT_INTERVAL);
    retry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; consume it up front so it does not
    // race the acquisition `machine.start()` just performed.
    retry_tick.tick().await;

    // Teaching note on what this loop deliberately does *not* do: each
    // branch body `await`s the machine to completion before the next
    // `select!`, so while the `LockPending` sequence is running (at most the
    // confirmation deadline, a few seconds) nothing else here is serviced.
    // That is intentional. The machine is a single `&mut` state machine —
    // concurrent event handling would mean two events racing the same
    // inhibitor — and signals that arrive meanwhile are not lost: zbus
    // buffers them in their streams and they are handled on the next turn,
    // by which point the already-locked check makes a second `Lock` a no-op
    // and the `ReadyToSleep` guard makes a second `PrepareForSleep(true)` a
    // no-op. The only cost is that a shutdown request can wait up to one
    // deadline, which is well inside systemd's stop timeout.
    loop {
        tokio::select! {
            // `biased` makes shutdown the first branch polled every time:
            // when systemd stops the unit we want to release the inhibitor
            // promptly, not after servicing whatever else is ready.
            biased;

            _ = shutdown.changed() => {
                info!("saola-session: sleep module stopping");
                break;
            }

            signal = next_signal(&mut prepare_for_sleep) => {
                match signal {
                    Some(signal) => match signal.args() {
                        Ok(args) => {
                            let start = args.start;
                            debug!(start, "saola-session: PrepareForSleep received");
                            machine.handle_event(SleepEvent::PrepareForSleep(start)).await;
                        }
                        Err(err) => warn!(
                            error = %err,
                            "saola-session: could not decode a PrepareForSleep signal"
                        ),
                    },
                    None => {
                        // The stream ended: our D-Bus connection is gone
                        // (the bus itself died — a logind restart alone does
                        // NOT end this stream, see `LogindSignals`'s doc
                        // comment; `logind_owner_changed` below is what
                        // catches that case). Staying alive here is the
                        // silent-absence failure `CLAUDE.md` calls worse
                        // than a crash — we would look running while never
                        // locking again. Return, so `main.rs` exits non-zero
                        // and systemd restarts us into a fresh connection.
                        error!(
                            "saola-session: the PrepareForSleep signal stream ended — the D-Bus \
                             connection is gone; stopping so the daemon is restarted rather than \
                             left half-alive"
                        );
                        break;
                    }
                }
            }

            signal = next_signal(&mut lock) => {
                match signal {
                    Some(_) => {
                        info!("saola-session: logind Lock signal received");
                        machine.handle_event(SleepEvent::Lock).await;
                    }
                    None => {
                        error!(
                            "saola-session: the session Lock signal stream ended — the D-Bus \
                             connection is gone; stopping so the daemon is restarted rather than \
                             left half-alive"
                        );
                        break;
                    }
                }
            }

            signal = next_signal(&mut unlock) => {
                match signal {
                    Some(_) => {
                        machine.handle_event(SleepEvent::Unlock).await;
                    }
                    None => {
                        error!(
                            "saola-session: the session Unlock signal stream ended — the D-Bus \
                             connection is gone; stopping so the daemon is restarted rather than \
                             left half-alive"
                        );
                        break;
                    }
                }
            }

            signal = next_signal(&mut logind_owner_changed) => {
                match signal {
                    Some(_) => {
                        machine.handle_event(SleepEvent::LogindRestarted).await;
                    }
                    None => {
                        error!(
                            "saola-session: the logind NameOwnerChanged stream ended — the D-Bus \
                             connection is gone; stopping so the daemon is restarted rather than \
                             left half-alive"
                        );
                        break;
                    }
                }
            }

            _ = retry_tick.tick(), if lock_before_sleep && !machine.holds_inhibitor() => {
                debug!(
                    "saola-session: periodic check found no sleep inhibitor held — retrying \
                     acquisition (E-5)"
                );
                // `ensure_inhibitor` is a private method of `SleepMachine`,
                // callable here because `run` lives in the same module —
                // no new public surface needed for a call `run` already has
                // every right to make.
                machine.ensure_inhibitor().await;
            }
        }
    }

    // Worth a line in the journal: "we stopped while `ReadyToSleep` and
    // holding nothing" and "we stopped `Awake` holding the inhibitor" are
    // very different things to have been true at shutdown.
    debug!(
        state = ?machine.state(),
        inhibitor_held = machine.holds_inhibitor(),
        "saola-session: sleep module final state"
    );
    machine.release_for_shutdown();
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// One externally-visible thing the machine did. The fakes append to a
    /// shared journal so tests can assert on *ordering* across both traits —
    /// which is the only way to check Architecture's "re-acquired on every
    /// resume before anything else".
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Step {
        Acquire,
        AcquireFailed,
        /// Pushed by the fake inhibitor's `Drop` — i.e. the actual release.
        Release,
        LockedHint(bool),
        LockedHintUnreadable,
        Spawn,
        SpawnFailed,
    }

    #[derive(Clone, Default)]
    struct Journal(Arc<Mutex<Vec<Step>>>);

    impl Journal {
        fn push(&self, step: Step) {
            if let Ok(mut steps) = self.0.lock() {
                steps.push(step);
            }
        }

        fn steps(&self) -> Vec<Step> {
            self.0
                .lock()
                .map(|steps| steps.clone())
                .unwrap_or_else(|_| Vec::new())
        }

        fn count(&self, step: &Step) -> usize {
            self.steps().iter().filter(|s| *s == step).count()
        }
    }

    /// Dropped when the fake inhibitor is released — the test double for
    /// "closing the fd releases the lock".
    struct ReleaseRecorder {
        journal: Journal,
    }

    impl Drop for ReleaseRecorder {
        fn drop(&mut self) {
            self.journal.push(Step::Release);
        }
    }

    /// What `LockedHint` answers, call by call.
    #[derive(Debug, Clone, Copy)]
    enum Hints {
        /// Never locked — drives the deadline path.
        NeverLocked,
        /// Already locked before we do anything.
        AlreadyLocked,
        /// `false` for the first `n` reads, `true` from then on. `n = 1`
        /// means: unlocked for the already-locked check, confirmed on the
        /// first confirmation poll.
        LockedAfter(usize),
        /// logind cannot be read at all.
        Unreadable,
    }

    struct FakeLogind {
        journal: Journal,
        hints: Hints,
        reads: AtomicUsize,
        acquire_fails: bool,
    }

    impl Logind for FakeLogind {
        fn acquire_delay_inhibitor(&self) -> BoxFuture<'_, Result<Inhibitor, LogindError>> {
            Box::pin(async move {
                if self.acquire_fails {
                    self.journal.push(Step::AcquireFailed);
                    return Err(LogindError::new("fake: logind refused the inhibitor"));
                }
                self.journal.push(Step::Acquire);
                Ok(Inhibitor::new(ReleaseRecorder {
                    journal: self.journal.clone(),
                }))
            })
        }

        fn locked_hint(&self) -> BoxFuture<'_, Result<bool, LogindError>> {
            Box::pin(async move {
                let read = self.reads.fetch_add(1, Ordering::SeqCst);
                let locked = match self.hints {
                    Hints::NeverLocked => Some(false),
                    Hints::AlreadyLocked => Some(true),
                    Hints::LockedAfter(n) => Some(read >= n),
                    Hints::Unreadable => None,
                };
                match locked {
                    Some(locked) => {
                        self.journal.push(Step::LockedHint(locked));
                        Ok(locked)
                    }
                    None => {
                        self.journal.push(Step::LockedHintUnreadable);
                        Err(LogindError::new("fake: logind unreachable"))
                    }
                }
            })
        }
    }

    struct FakeSpawner {
        journal: Journal,
        fails: bool,
    }

    impl LockerSpawner for FakeSpawner {
        fn spawn_locker(&self) -> BoxFuture<'_, Result<(), SpawnError>> {
            Box::pin(async move {
                if self.fails {
                    self.journal.push(Step::SpawnFailed);
                    return Err(SpawnError::new("fake: no such locker"));
                }
                self.journal.push(Step::Spawn);
                Ok(())
            })
        }
    }

    /// Builds a [`SleepMachine`] over the fakes. Defaults are the happy
    /// path; each method turns one thing sour.
    struct Rig {
        hints: Hints,
        spawn_fails: bool,
        acquire_fails: bool,
        lock_before_sleep: bool,
    }

    impl Rig {
        fn new(hints: Hints) -> Self {
            Rig {
                hints,
                spawn_fails: false,
                acquire_fails: false,
                lock_before_sleep: true,
            }
        }

        fn spawn_fails(mut self) -> Self {
            self.spawn_fails = true;
            self
        }

        fn acquire_fails(mut self) -> Self {
            self.acquire_fails = true;
            self
        }

        fn lock_before_sleep(mut self, enabled: bool) -> Self {
            self.lock_before_sleep = enabled;
            self
        }

        fn build(self) -> (SleepMachine, Journal) {
            let journal = Journal::default();
            let logind: Arc<dyn Logind> = Arc::new(FakeLogind {
                journal: journal.clone(),
                hints: self.hints,
                reads: AtomicUsize::new(0),
                acquire_fails: self.acquire_fails,
            });
            let spawner: Arc<dyn LockerSpawner> = Arc::new(FakeSpawner {
                journal: journal.clone(),
                fails: self.spawn_fails,
            });
            let locker = SessionLocker::new(Arc::clone(&logind), spawner);
            let machine = SleepMachine::new(
                logind,
                locker,
                self.lock_before_sleep,
                DEFAULT_CONFIRMATION_DEADLINE,
            );
            (machine, journal)
        }
    }

    // ---------------------------------------------------------------- startup

    #[tokio::test(start_paused = true)]
    async fn startup_takes_the_delay_inhibitor() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked).build();
        machine.start().await;

        assert!(machine.holds_inhibitor());
        assert_eq!(machine.state(), SleepState::Awake);
        assert_eq!(journal.steps(), vec![Step::Acquire]);
    }

    #[tokio::test(start_paused = true)]
    async fn startup_skips_the_inhibitor_when_lock_before_sleep_is_disabled() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked)
            .lock_before_sleep(false)
            .build();
        machine.start().await;

        assert!(!machine.holds_inhibitor());
        assert_eq!(journal.steps(), Vec::new());
    }

    #[tokio::test(start_paused = true)]
    async fn inhibitor_acquisition_failure_is_retried_then_survived() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked).acquire_fails().build();
        machine.start().await;

        // Retried the full budget, gave up, and did not panic or die.
        assert!(!machine.holds_inhibitor());
        assert_eq!(
            journal.count(&Step::AcquireFailed),
            INHIBITOR_ACQUIRE_ATTEMPTS as usize
        );

        // ...and the daemon still locks on request, which is the whole point
        // of not treating this as fatal.
        let outcome = machine.handle_event(SleepEvent::Lock).await;
        assert_eq!(outcome, EventOutcome::Locked(LockAttempt::Spawned));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_releases_the_inhibitor() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked).build();
        machine.start().await;
        machine.release_for_shutdown();

        assert!(!machine.holds_inhibitor());
        assert_eq!(journal.steps(), vec![Step::Acquire, Step::Release]);
    }

    // ------------------------------------------------------- before-sleep path

    #[tokio::test(start_paused = true)]
    async fn confirmation_arrives_before_the_deadline() {
        // Read 0 is the already-locked check (false → spawn); read 1 is the
        // first confirmation poll, which comes back true.
        let (mut machine, journal) = Rig::new(Hints::LockedAfter(1)).build();
        machine.start().await;

        let outcome = machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;

        assert_eq!(
            outcome,
            EventOutcome::PreparedForSleep(SleepReadiness::Confirmed)
        );
        assert_eq!(machine.state(), SleepState::ReadyToSleep);
        assert!(!machine.holds_inhibitor());
        assert_eq!(
            journal.steps(),
            vec![
                Step::Acquire,
                Step::LockedHint(false),
                Step::Spawn,
                Step::LockedHint(true),
                Step::Release,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn confirmation_times_out_and_sleep_proceeds_anyway() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked).build();
        machine.start().await;

        let outcome = machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;

        // Severity rule 2: the inhibitor is released on the deadline even
        // though nothing ever confirmed.
        assert_eq!(
            outcome,
            EventOutcome::PreparedForSleep(SleepReadiness::Unconfirmed)
        );
        assert_eq!(machine.state(), SleepState::ReadyToSleep);
        assert!(!machine.holds_inhibitor());
        assert_eq!(journal.count(&Step::Release), 1);
        assert_eq!(journal.count(&Step::Spawn), 1);
        // It really did keep polling for the whole deadline rather than
        // giving up after one read: 2 s / 50 ms polls, plus the
        // already-locked check.
        assert!(journal.count(&Step::LockedHint(false)) > 10);
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_failure_still_releases_the_inhibitor() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked).spawn_fails().build();
        machine.start().await;

        let outcome = machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;

        assert_eq!(
            outcome,
            EventOutcome::PreparedForSleep(SleepReadiness::SpawnFailed)
        );
        assert!(!machine.holds_inhibitor());
        // Released immediately: no confirmation polling, because nothing was
        // started that could ever confirm.
        assert_eq!(
            journal.steps(),
            vec![
                Step::Acquire,
                Step::LockedHint(false),
                Step::SpawnFailed,
                Step::Release,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn already_locked_skips_the_spawn() {
        let (mut machine, journal) = Rig::new(Hints::AlreadyLocked).build();
        machine.start().await;

        let outcome = machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;

        assert_eq!(
            outcome,
            EventOutcome::PreparedForSleep(SleepReadiness::AlreadyLocked)
        );
        assert_eq!(journal.count(&Step::Spawn), 0);
        assert_eq!(
            journal.steps(),
            vec![Step::Acquire, Step::LockedHint(true), Step::Release]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unreadable_locked_hint_is_treated_as_unlocked() {
        // Severity rule 1 beats rule 3: if we cannot tell, we lock.
        let (mut machine, journal) = Rig::new(Hints::Unreadable).build();
        machine.start().await;

        let outcome = machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;

        assert_eq!(
            outcome,
            EventOutcome::PreparedForSleep(SleepReadiness::Unconfirmed)
        );
        assert_eq!(journal.count(&Step::Spawn), 1);
        assert_eq!(journal.count(&Step::Release), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn double_prepare_for_sleep_does_not_stack_lockers() {
        let (mut machine, journal) = Rig::new(Hints::LockedAfter(1)).build();
        machine.start().await;

        machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;
        let second = machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;

        assert_eq!(second, EventOutcome::NoAction);
        assert_eq!(machine.state(), SleepState::ReadyToSleep);
        assert_eq!(journal.count(&Step::Spawn), 1);
        assert_eq!(journal.count(&Step::Release), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn prepare_for_sleep_is_inert_when_lock_before_sleep_is_disabled() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked)
            .lock_before_sleep(false)
            .build();
        machine.start().await;

        let outcome = machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;

        assert_eq!(outcome, EventOutcome::NoAction);
        assert_eq!(journal.steps(), Vec::new());
    }

    // ------------------------------------------------------------ resume path

    #[tokio::test(start_paused = true)]
    async fn resume_reacquires_the_inhibitor_before_anything_else() {
        let (mut machine, journal) = Rig::new(Hints::LockedAfter(1)).build();
        machine.start().await;
        machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;

        let outcome = machine
            .handle_event(SleepEvent::PrepareForSleep(false))
            .await;

        assert_eq!(
            outcome,
            EventOutcome::Resumed {
                inhibitor_held: true
            }
        );
        assert_eq!(machine.state(), SleepState::Awake);
        assert!(machine.holds_inhibitor());
        // The ordering assertion that matters: the second `Acquire` is the
        // very next thing after the `Release`, with nothing between it and
        // the resume event.
        assert_eq!(
            journal.steps(),
            vec![
                Step::Acquire,
                Step::LockedHint(false),
                Step::Spawn,
                Step::LockedHint(true),
                Step::Release,
                Step::Acquire,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resume_reports_a_failed_reacquire() {
        let (mut machine, _journal) = Rig::new(Hints::NeverLocked).acquire_fails().build();
        machine.start().await;

        let outcome = machine
            .handle_event(SleepEvent::PrepareForSleep(false))
            .await;

        assert_eq!(
            outcome,
            EventOutcome::Resumed {
                inhibitor_held: false
            }
        );
        assert_eq!(machine.state(), SleepState::Awake);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_sleep_resume_cycle_can_sleep_again() {
        // The regression this guards: if resume did not reset the state out
        // of `ReadyToSleep`, the *second* suspend would be swallowed by the
        // duplicate-`true` guard and never lock.
        let (mut machine, journal) = Rig::new(Hints::LockedAfter(1)).build();
        machine.start().await;
        machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;
        machine
            .handle_event(SleepEvent::PrepareForSleep(false))
            .await;

        let outcome = machine
            .handle_event(SleepEvent::PrepareForSleep(true))
            .await;

        // `LockedAfter(1)` means every read from the second onwards is
        // `true`, so the second cycle sees an already-locked session (the
        // user never unlocked) and correctly declines to stack a locker.
        assert_eq!(
            outcome,
            EventOutcome::PreparedForSleep(SleepReadiness::AlreadyLocked)
        );
        assert_eq!(journal.count(&Step::Spawn), 1);
        assert_eq!(journal.count(&Step::Acquire), 2);
        assert_eq!(journal.count(&Step::Release), 2);
    }

    // ------------------------------------------------------- Lock/Unlock path

    #[tokio::test(start_paused = true)]
    async fn lock_signal_spawns_the_locker() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked).build();
        machine.start().await;

        let outcome = machine.handle_event(SleepEvent::Lock).await;

        assert_eq!(outcome, EventOutcome::Locked(LockAttempt::Spawned));
        assert_eq!(journal.count(&Step::Spawn), 1);
        // A `Lock` signal must not touch the inhibitor or the sleep state.
        assert!(machine.holds_inhibitor());
        assert_eq!(machine.state(), SleepState::Awake);
    }

    #[tokio::test(start_paused = true)]
    async fn lock_signal_while_locked_does_not_spawn() {
        let (mut machine, journal) = Rig::new(Hints::AlreadyLocked).build();
        machine.start().await;

        let outcome = machine.handle_event(SleepEvent::Lock).await;

        assert_eq!(outcome, EventOutcome::Locked(LockAttempt::AlreadyLocked));
        assert_eq!(journal.count(&Step::Spawn), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn lock_signal_is_honored_even_when_lock_before_sleep_is_disabled() {
        // `lock-before-sleep #false` turns off the *sleep* concern, not
        // `loginctl lock-session`.
        let (mut machine, journal) = Rig::new(Hints::NeverLocked)
            .lock_before_sleep(false)
            .build();
        machine.start().await;

        let outcome = machine.handle_event(SleepEvent::Lock).await;

        assert_eq!(outcome, EventOutcome::Locked(LockAttempt::Spawned));
        assert_eq!(journal.count(&Step::Spawn), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn unlock_signal_takes_no_action() {
        let (mut machine, journal) = Rig::new(Hints::AlreadyLocked).build();
        machine.start().await;

        let outcome = machine.handle_event(SleepEvent::Unlock).await;

        assert_eq!(outcome, EventOutcome::NoAction);
        assert_eq!(journal.steps(), vec![Step::Acquire]);
        assert!(machine.holds_inhibitor());
    }

    // ------------------------------------------------------- LogindRestarted
    // (Stage 7's finding E-2): the machine-level half of the fix. The
    // `run()`-level half — noticing the restart via `NameOwnerChanged` — is
    // D-Bus plumbing in the same sense the Wayland thread is, and is not
    // unit-tested for the same reason (see this module's doc comment on the
    // trait boundary); what *is* tested here is that once the event reaches
    // the machine, a stale fd is actually dropped before a fresh one is
    // requested, not left in place by `ensure_inhibitor`'s `is_some()`
    // short-circuit.

    #[tokio::test(start_paused = true)]
    async fn logind_restart_discards_a_stale_inhibitor_and_reacquires() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked).build();
        machine.start().await;
        assert!(machine.holds_inhibitor());

        let outcome = machine.handle_event(SleepEvent::LogindRestarted).await;

        assert_eq!(
            outcome,
            EventOutcome::LogindRestarted {
                inhibitor_held: true
            }
        );
        assert!(machine.holds_inhibitor());
        // The load-bearing assertion: `Release` happens before the second
        // `Acquire` — i.e. the stale fd was actually dropped, not just
        // shadowed by a check that saw "something is held" and stopped
        // looking.
        assert_eq!(
            journal.steps(),
            vec![Step::Acquire, Step::Release, Step::Acquire]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn logind_restart_with_reacquire_failure_reports_not_held() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked).acquire_fails().build();
        // Nothing held to begin with (acquisition fails), so this also
        // covers "no prior inhibitor" — release is a harmless no-op.
        let outcome = machine.handle_event(SleepEvent::LogindRestarted).await;

        assert_eq!(
            outcome,
            EventOutcome::LogindRestarted {
                inhibitor_held: false
            }
        );
        assert!(!machine.holds_inhibitor());
        assert_eq!(
            journal.count(&Step::AcquireFailed),
            INHIBITOR_ACQUIRE_ATTEMPTS as usize
        );
    }

    #[tokio::test(start_paused = true)]
    async fn logind_restart_is_inert_when_lock_before_sleep_is_disabled() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked)
            .lock_before_sleep(false)
            .build();

        let outcome = machine.handle_event(SleepEvent::LogindRestarted).await;

        assert_eq!(outcome, EventOutcome::NoAction);
        assert_eq!(journal.steps(), Vec::new());
    }

    // ------------------------------------------- cross-task double-spawn (E-3)

    /// The regression Stage 7's finding E-3 describes directly: `idle.rs`'s
    /// task and this module's task each hold a clone of the same
    /// `SessionLocker`. If both observe `LockedHint == false` inside the
    /// ~360 ms window before a spawn's effect is visible (`docs/SIGNALS.md`
    /// §1), a naive implementation spawns twice. `tokio::join!` drives two
    /// clones' `lock_if_needed` calls concurrently on one task, which is
    /// enough to exercise the shared `last_spawn` mutex's serialisation
    /// without needing real threads.
    #[tokio::test(start_paused = true)]
    async fn concurrent_clones_do_not_stack_lockers_inside_the_settle_grace() {
        let journal = Journal::default();
        let logind: Arc<dyn Logind> = Arc::new(FakeLogind {
            journal: journal.clone(),
            hints: Hints::NeverLocked,
            reads: AtomicUsize::new(0),
            acquire_fails: false,
        });
        let spawner: Arc<dyn LockerSpawner> = Arc::new(FakeSpawner {
            journal: journal.clone(),
            fails: false,
        });
        let locker_a = SessionLocker::new(Arc::clone(&logind), spawner);
        let locker_b = locker_a.clone();

        let (a, b) = tokio::join!(
            locker_a.lock_if_needed(LockTrigger::LogindLockSignal),
            locker_b.lock_if_needed(LockTrigger::LogindLockSignal),
        );

        // Exactly one clone actually spawned a locker...
        assert_eq!(journal.count(&Step::Spawn), 1);
        // ...and the outcomes reflect that: one `Spawned`, one
        // `SpawnPending` — never two `Spawned`.
        let outcomes = [a, b];
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| **o == LockAttempt::Spawned)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| **o == LockAttempt::SpawnPending)
                .count(),
            1
        );
    }

    /// Once the settle grace has elapsed, a clone that saw `SpawnPending`
    /// earlier is free to spawn again if the session is genuinely still
    /// unlocked (e.g. the earlier locker died before setting `LockedHint`).
    #[tokio::test(start_paused = true)]
    async fn a_second_spawn_is_allowed_once_the_settle_grace_elapses() {
        let (mut machine, journal) = Rig::new(Hints::NeverLocked).build();
        // Route both calls through the same underlying `SessionLocker` the
        // way `main.rs` would (one instance, cloned) — here just reused
        // directly via the machine's `Lock` event twice, with the grace
        // window allowed to pass in between.
        machine.handle_event(SleepEvent::Lock).await;
        tokio::time::sleep(LOCK_SETTLE_GRACE + Duration::from_millis(10)).await;
        let outcome = machine.handle_event(SleepEvent::Lock).await;

        assert_eq!(outcome, EventOutcome::Locked(LockAttempt::Spawned));
        assert_eq!(journal.count(&Step::Spawn), 2);
    }

    // ------------------------------------------------------------ pure helpers

    #[test]
    fn the_deadline_defaults_when_logind_will_not_say() {
        assert_eq!(confirmation_deadline(None), DEFAULT_CONFIRMATION_DEADLINE);
    }

    #[test]
    fn the_deadline_uses_four_fifths_of_this_machines_five_second_cap() {
        // 4/5 of 5 s is 4 s, which now equals the default exactly (Stage 7's
        // finding E-6: the old 3/5 ratio never actually bound on this
        // machine because the then-2s default always won first).
        assert_eq!(
            confirmation_deadline(Some(Duration::from_secs(5))),
            DEFAULT_CONFIRMATION_DEADLINE
        );
    }

    #[test]
    fn the_deadline_is_clamped_under_a_tighter_cap() {
        // 4/5 of 1 s is 800 ms — comfortably inside the cap.
        assert_eq!(
            confirmation_deadline(Some(Duration::from_secs(1))),
            Duration::from_millis(800)
        );
    }

    #[test]
    fn an_absurd_cap_cannot_overflow_the_deadline() {
        // `Duration` arithmetic panics on overflow; the no-panic rule means
        // a hostile or corrupt property value must degrade, not abort.
        assert_eq!(
            confirmation_deadline(Some(Duration::MAX)),
            DEFAULT_CONFIRMATION_DEADLINE
        );
    }

    #[test]
    fn a_bare_locker_command_has_no_arguments() {
        assert_eq!(
            split_command("saola-lockscreen"),
            Some(("saola-lockscreen".to_string(), Vec::new()))
        );
    }

    #[test]
    fn a_locker_command_keeps_its_arguments_in_order() {
        assert_eq!(
            split_command("  saola-lockscreen  --grace 5 "),
            Some((
                "saola-lockscreen".to_string(),
                vec!["--grace".to_string(), "5".to_string()]
            ))
        );
    }

    #[test]
    fn an_empty_locker_command_is_not_spawnable() {
        assert_eq!(split_command("   "), None);
    }
}
