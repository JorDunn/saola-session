//! `ext-idle-notify-v1` idle policy — Stage 5.
//!
//! Implements Architecture's idle-policy block (`PLAN.md`): two independent
//! timeouts, `lock` and `power-off`, each backed by its own
//! `ext_idle_notification_v1` object. `lock` reuses `modules::sleep`'s single
//! locker-spawn path (`SessionLocker::lock_if_needed`); `power-off` runs
//! `niri msg action power-off-monitors`. Both re-arm on the protocol's
//! `resumed` event. See Stage 2's handoff (`docs/SIGNALS.md` §5) for why this
//! module's live-test plan (a nested niri) is viable: a nested compositor
//! advertises `ext_idle_notifier_v1` just like a real one.
//!
//! # Two halves, same shape as `sleep.rs`
//!
//! - **[`IdlePolicy`]** is the pure state machine Architecture calls for:
//!   "policy is a pure state machine over injected events". It knows nothing
//!   about Wayland, D-Bus, or `tokio::process` — only [`IdleEvent`] in,
//!   [`IdleOutcome`] out. Every behavior the task list asks for (each timeout
//!   fires once, resume re-arms, inhibit suppresses new actions but not
//!   in-flight ones, disabled actions never fire) is a unit test against this
//!   type alone, at the bottom of this file.
//! - **[`IdleExecutor`]** turns an [`IdleOutcome`] into the two real actions
//!   ([`crate::modules::sleep::SessionLocker::lock_if_needed`] and
//!   [`PowerOffCommand::power_off_monitors`]), behind the same small-trait
//!   pattern `sleep.rs` uses for `Logind`/`LockerSpawner` — so the executor,
//!   like the policy, is unit-testable without a real `niri msg` process.
//!
//! Neither of those two types knows anything about Wayland. The Wayland
//! plumbing ([`run`], [`wayland_thread_main`]) is the third piece, and it is
//! deliberately *not* unit-tested (Architecture: "the Wayland and D-Bus
//! plumbing feed it, tests drive it directly" — "it" being the policy
//! machine) — it is exercised live, in a nested niri, per this crate's
//! nested-niri testing rule.
//!
//! # The synchronous-Wayland-to-async bridge (teaching note)
//!
//! `wayland-client` is not async: an [`wayland_client::EventQueue`]'s
//! [`wayland_client::EventQueue::blocking_dispatch`] parks the calling thread
//! until the compositor has something to say, with no `Future` anywhere in
//! that picture. That is the same shape `saola-panel`'s `modules/volume.rs`
//! hit with libpulse's mainloop (see that module's doc comment for the fuller
//! version of this note), and the fix is the same one: **a dedicated
//! `std::thread`**, not a tokio task. [`run`] spawns that thread once, then
//! never touches Wayland itself — it only reads [`IdleEvent`]s off a
//! [`tokio::sync::mpsc::UnboundedSender`] channel the thread's [`Dispatch`]
//! impl feeds. `UnboundedSender::send` is a plain synchronous method (unlike
//! the *bounded* channel's `send`, which is an `async fn` and therefore
//! unusable from a thread with no executor under it) — exactly why the panel
//! picked the same primitive for its own C-callback-to-async bridge.
//!
//! One simplification versus the panel's pattern, called out explicitly
//! rather than left to be discovered later: the panel's `volume.rs` adds a
//! **self-pipe** so its worker thread's blocking wait can be woken early by a
//! command from the async side. This module does not need that, because
//! nothing ever needs to *tell* the Wayland thread anything after startup —
//! idle notifications are compositor-driven, one direction only. On
//! shutdown, [`run`] simply stops reading the channel and returns; the
//! Wayland thread is left blocked inside the kernel's `read()` on a socket
//! that will be closed when the process exits, using no CPU in the meantime.
//! It is intentionally **not joined** — see [`run`]'s doc comment for the
//! severity reasoning behind that choice.

use std::fmt;
use std::process::Stdio;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error, info, warn};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

use crate::config::IdleConfig;
use crate::modules::sleep::{BoxFuture, LockTrigger, SessionLocker};

/// How long [`IdleExecutor`] waits for `niri msg action power-off-monitors`
/// before giving up on that one attempt. Architecture's spawn-hygiene rule
/// (severity 3: a stuck `niri msg` must never block this module's event
/// loop, which would delay the *next* idle/resume/inhibit event it needs to
/// react to) — bounded the same defensive way every other external call in
/// this crate is (`sleep.rs`'s confirmation deadline, its inhibitor-acquire
/// retry budget).
const POWER_OFF_TIMEOUT: Duration = Duration::from_secs(5);

/// How many times [`wayland_thread_main`] retries the *initial* Wayland
/// connect before giving up — Stage 7's finding E-4. `graphical-session.target`
/// being reached does not guarantee niri's socket is accepting connections
/// yet, nor that `WAYLAND_DISPLAY` has been imported into the user manager's
/// environment (a separate, later, racy step) — so a connect failure right
/// at startup is often a lost *race*, not a real absence of a compositor.
/// That is different from a *later* dispatch failure (handled separately,
/// inline in the dispatch loop), which really does mean the compositor
/// died, where dying immediately remains correct. Retrying here instead of
/// exiting on the first failure is what stops systemd's default start-rate
/// limiter (`StartLimitBurst=5` over `StartLimitIntervalSec=10s`) from
/// turning one lost boot race into a *permanently* `failed` unit — see
/// `contrib/systemd/saola-session.service`'s own comment on the matching
/// unit-level half of this fix (`RestartSec`/`StartLimitIntervalSec`).
const STARTUP_CONNECT_ATTEMPTS: u32 = 10;

/// Backoff between startup connect attempts. `10 × 2 s` = 20 s of total
/// retry budget — long enough to ride out a slow boot, short enough to stay
/// comfortably inside systemd's default 90 s service stop timeout. That
/// bound matters here specifically: this loop runs on a plain `std::thread`
/// with no shutdown signal wired to it (see the module doc comment's
/// "synchronous-Wayland-to-async bridge" section for why there is no
/// self-pipe), so a shutdown that arrives mid-retry cannot cancel it early
/// — `run`'s `ready_rx.await` simply waits the loop out. Keeping the total
/// budget well under the stop timeout is what makes that an acceptable,
/// bounded cost instead of a hang.
const STARTUP_CONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// [`Duration`] → the `u32` milliseconds
/// `ext_idle_notifier_v1.get_idle_notification`'s `timeout` argument wants.
/// `config.rs` already restricts `session.toml`'s timeouts to positive
/// values, but this crate's no-panic rule means an operator writing an
/// enormous number (u32 milliseconds tops out around 49.7 days) must
/// saturate to the protocol's maximum, not panic or wrap.
fn timeout_millis(timeout: Duration) -> u32 {
    u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX)
}

// ============================================================================
// The pure policy state machine — unit-tested directly, no Wayland involved.
// ============================================================================

/// Which `ext_idle_notification_v1` a given event or action is about. Two
/// independent notifications, per Architecture — a machine can have
/// `lock-after` configured without `power-off-after`, or vice versa, and each
/// runs its own timeout against the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdleTarget {
    /// `idle.lock-after` — spawn-if-not-locked on timeout.
    Lock,
    /// `idle.power-off-after` — `niri msg action power-off-monitors` on
    /// timeout.
    PowerOff,
}

impl fmt::Display for IdleTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdleTarget::Lock => f.write_str("lock"),
            IdleTarget::PowerOff => f.write_str("power-off"),
        }
    }
}

/// One thing that happened, fed to [`IdlePolicy::handle`]. [`run`] translates
/// the Wayland thread's channel messages and the inhibit-active watch
/// channel into these; tests construct them directly — same shape as
/// `sleep.rs`'s [`crate::modules::sleep::SleepEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    /// The notification's `idled` event: the seat has been inactive for at
    /// least its configured timeout.
    Idled(IdleTarget),
    /// The notification's `resumed` event: activity started again. Also what
    /// re-arms the target so its *next* idle period can fire again.
    Resumed(IdleTarget),
    /// **Stage 6's plug-in point.** An active `org.freedesktop.ScreenSaver`
    /// inhibit changed. Not yet fed by anything real — Stage 6 doesn't exist
    /// yet — but the input exists now, defaults to `false` (no inhibit,
    /// [`IdlePolicy::new`]), and is already wired end-to-end through [`run`]
    /// from a [`watch::Receiver<bool>`] parameter. See [`run`]'s doc comment
    /// for exactly where Stage 6 connects its sender.
    InhibitChanged(bool),
}

/// What [`IdlePolicy::handle`] did with one event. Returned rather than only
/// logged so tests assert on behavior, not log scraping — same reasoning as
/// `sleep.rs`'s `EventOutcome`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleOutcome {
    /// Nothing to do: a disabled target, a duplicate `Idled` without an
    /// intervening `Resumed` (a compositor protocol violation per the
    /// `ext-idle-notify-v1` spec, but this machine tolerates it rather than
    /// trusting the compositor to be conformant), or a `Resumed` for a
    /// disabled target.
    NoAction,
    /// `Idled` arrived while an inhibit was active — the target is marked
    /// [`Arm::Suppressed`] (so it does not re-fire until the next
    /// `Resumed`/`Idled` cycle, but — Stage 7's finding F-1 — *is*
    /// re-evaluated the moment the inhibit clears), and its action is *not*
    /// run.
    Suppressed(IdleTarget),
    /// `Idled` arrived, no inhibit was active: the target's action should
    /// run. [`IdleExecutor::execute`] is what actually runs it.
    Fired(IdleTarget),
    /// `Resumed` re-armed a target that had fired (or been suppressed).
    Rearmed(IdleTarget),
    /// The inhibit-active flag changed, and nothing was owed a fire because
    /// of it (either it just became active, or it cleared with no target
    /// left `Suppressed`). Never itself cancels or fires anything
    /// (Architecture: "never cancels an in-flight lock") — it only changes
    /// what a *future* `Idled` does.
    InhibitChanged(bool),
    /// Stage 7's finding F-1: the inhibit cleared (`InhibitChanged(false)`)
    /// while one or more targets were sitting `Suppressed`. Before this
    /// fix, the *only* thing that could ever re-fire a suppressed target
    /// was a real `Resumed` — i.e. someone touching the keyboard — which
    /// meant "walk away, a video inhibits idle, the video ends, nobody
    /// comes back" left the session unlocked all night (severity 1, the
    /// exact scenario `org.freedesktop.ScreenSaver` exists for). The seat
    /// has *already* been idle longer than each fired target's configured
    /// timeout, so firing them now is not a spurious lock — it is the lock
    /// that was owed and deferred. Always starts with
    /// `InhibitChanged(false)`, followed by one `Fired(target)` per target
    /// that was `Suppressed`; [`IdleExecutor::execute`] runs each entry in
    /// turn.
    Multiple(Vec<IdleOutcome>),
}

/// One target's arm state: has its current idle period already fired, is it
/// waiting on an inhibit to clear before it can, or is it still waiting to
/// idle out at all?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    Armed,
    Fired,
    /// `Idled` arrived while inhibited. Distinct from `Fired` (Stage 7's
    /// finding F-1) specifically so `InhibitChanged(false)` can tell "this
    /// target already ran its action" from "this target is still owed one"
    /// — collapsing the two into a single `Fired` state is exactly the bug
    /// the finding describes.
    Suppressed,
}

/// Architecture's idle policy state machine. Owns nothing external — no
/// Wayland proxy, no `SessionLocker`, no clock. Just two optional [`Arm`]s
/// (`None` means that target is disabled — `session.toml` never configured a
/// timeout for it, so it can never appear as an event and could never fire
/// even if it somehow did) and the current inhibit-active flag.
pub struct IdlePolicy {
    lock: Option<Arm>,
    power_off: Option<Arm>,
    inhibit_active: bool,
}

impl IdlePolicy {
    /// Builds the machine from `session.toml`'s `[idle]` table. A target
    /// starts `Some(Arm::Armed)` if its timeout is `Some`, `None`
    /// (permanently disabled) otherwise — mirroring `config.rs`'s own
    /// "`None` means disabled, not some fallback timeout" rule.
    pub fn new(idle: &IdleConfig) -> Self {
        IdlePolicy {
            lock: idle.lock_after.map(|_| Arm::Armed),
            power_off: idle.power_off_after.map(|_| Arm::Armed),
            inhibit_active: false,
        }
    }

    fn slot(&mut self, target: IdleTarget) -> Option<&mut Arm> {
        match target {
            IdleTarget::Lock => self.lock.as_mut(),
            IdleTarget::PowerOff => self.power_off.as_mut(),
        }
    }

    /// Feeds one event through the machine. Pure and synchronous — no
    /// `.await` anywhere in here, which is exactly what makes the unit tests
    /// below able to drive it without a runtime.
    pub fn handle(&mut self, event: IdleEvent) -> IdleOutcome {
        match event {
            IdleEvent::InhibitChanged(active) => {
                self.inhibit_active = active;
                if active {
                    return IdleOutcome::InhibitChanged(true);
                }

                // F-1: the inhibit just cleared. Any target sitting
                // `Suppressed` is owed a fire it never got — re-evaluate
                // both slots rather than waiting for a `Resumed` that, per
                // the finding's failure scenario, may never arrive.
                let mut outcomes = vec![IdleOutcome::InhibitChanged(false)];
                for target in [IdleTarget::Lock, IdleTarget::PowerOff] {
                    if let Some(arm) = self.slot(target)
                        && *arm == Arm::Suppressed
                    {
                        *arm = Arm::Fired;
                        outcomes.push(IdleOutcome::Fired(target));
                    }
                }
                if outcomes.len() == 1 {
                    IdleOutcome::InhibitChanged(false)
                } else {
                    IdleOutcome::Multiple(outcomes)
                }
            }
            IdleEvent::Resumed(target) => match self.slot(target) {
                Some(arm) => {
                    *arm = Arm::Armed;
                    IdleOutcome::Rearmed(target)
                }
                None => IdleOutcome::NoAction,
            },
            IdleEvent::Idled(target) => {
                let inhibited = self.inhibit_active;
                match self.slot(target) {
                    None => IdleOutcome::NoAction,
                    // Already fired or suppressed this idle period and no
                    // Resumed arrived since — a duplicate `idled`, which the
                    // protocol says the compositor must never send but which
                    // this machine still treats as a no-op rather than
                    // trusting that guarantee.
                    Some(Arm::Fired) | Some(Arm::Suppressed) => IdleOutcome::NoAction,
                    Some(arm @ Arm::Armed) => {
                        if inhibited {
                            *arm = Arm::Suppressed;
                            IdleOutcome::Suppressed(target)
                        } else {
                            *arm = Arm::Fired;
                            IdleOutcome::Fired(target)
                        }
                    }
                }
            }
        }
    }
}

// ============================================================================
// Executing an outcome — the two real actions, behind small traits (the
// `sleep.rs` `Logind`/`LockerSpawner` pattern) so this half is unit-testable
// too, without a real `niri msg` child process.
// ============================================================================

/// A `niri msg action power-off-monitors` call failed or could not be
/// started. Stringly-typed, same reasoning as `sleep.rs`'s `SpawnError`.
#[derive(Debug)]
pub struct PowerOffError(String);

impl PowerOffError {
    fn new(message: impl Into<String>) -> Self {
        PowerOffError(message.into())
    }
}

impl fmt::Display for PowerOffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PowerOffError {}

/// The one thing this module needs beyond `sleep.rs`'s `SessionLocker`:
/// running `niri msg action power-off-monitors`. A trait (rather than a bare
/// function) purely so tests can swap in a fake, matching `sleep.rs`'s
/// `LockerSpawner` shape exactly — this is a one-method sibling of it.
pub trait PowerOffCommand: Send + Sync + 'static {
    fn power_off_monitors(&self) -> BoxFuture<'_, Result<(), PowerOffError>>;
}

/// The real [`PowerOffCommand`]: `tokio::process::Command`, awaited (unlike
/// `sleep.rs`'s locker spawn, this child is short-lived by design — `niri
/// msg` sends an IPC request and exits — so waiting for its exit status is
/// how we learn whether niri actually heard it, not a hygiene violation of
/// Architecture's "never waits for the locker to exit" rule, which is about
/// the *locker*, a long-running foreground process, specifically).
pub struct NiriPowerOff;

impl PowerOffCommand for NiriPowerOff {
    fn power_off_monitors(&self) -> BoxFuture<'_, Result<(), PowerOffError>> {
        Box::pin(async {
            let mut command = tokio::process::Command::new("niri");
            command.args(["msg", "action", "power-off-monitors"]);
            command.stdin(Stdio::null());
            // We do read the exit status (unlike the locker spawn) but never
            // stdout/stderr — `niri msg` diagnostics belong in the journal,
            // same as everything else this daemon spawns.
            command.stdout(Stdio::null());
            command.stderr(Stdio::null());

            match command.status().await {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => Err(PowerOffError::new(format!(
                    "niri msg action power-off-monitors exited with {status}"
                ))),
                Err(err) => Err(PowerOffError::new(format!(
                    "could not start 'niri msg action power-off-monitors': {err}"
                ))),
            }
        })
    }
}

/// Turns an [`IdleOutcome`] into the real action, or a log line for the
/// outcomes that are not actions at all. This is the layer Stage 6 does
/// *not* touch — its plug-in point is earlier, at [`IdleEvent::InhibitChanged`]
/// feeding [`IdlePolicy::handle`], not here.
pub struct IdleExecutor {
    locker: SessionLocker,
    power_off: Arc<dyn PowerOffCommand>,
}

impl IdleExecutor {
    pub fn new(locker: SessionLocker, power_off: Arc<dyn PowerOffCommand>) -> Self {
        IdleExecutor { locker, power_off }
    }

    /// Runs whatever `outcome` calls for. Every arm is `async` because both
    /// real actions are (`SessionLocker::lock_if_needed`, a process spawn),
    /// but the pure-logging arms complete immediately.
    pub async fn execute(&self, outcome: IdleOutcome) {
        match outcome {
            // F-1: run each sub-outcome in turn — always an
            // `InhibitChanged(false)` followed by the `Fired(target)`s it
            // owed. `Box::pin` is what makes this legal: an `async fn`
            // cannot call itself directly (the compiler would need to know
            // its own future's size, which would be infinite), but boxing
            // this one recursive call breaks that cycle. `Multiple` is never
            // nested (`IdlePolicy::handle` never builds one containing
            // another), so this recurses exactly one level deep.
            IdleOutcome::Multiple(outcomes) => {
                for outcome in outcomes {
                    Box::pin(self.execute(outcome)).await;
                }
            }
            IdleOutcome::NoAction => {}
            IdleOutcome::Suppressed(target) => {
                info!(
                    %target,
                    "saola-session: idle timeout suppressed by an active ScreenSaver inhibit"
                );
            }
            IdleOutcome::Rearmed(target) => {
                debug!(%target, "saola-session: idle target re-armed after activity resumed");
            }
            IdleOutcome::InhibitChanged(active) => {
                info!(
                    active,
                    "saola-session: idle inhibit-active flag changed (Stage 6's input)"
                );
            }
            IdleOutcome::Fired(IdleTarget::Lock) => {
                // Reuses `sleep.rs`'s single locker-spawn path — Architecture:
                // "one implementation, not two". `lock_if_needed` already
                // does its own already-locked check and logging; nothing
                // further to do with the return value here.
                self.locker.lock_if_needed(LockTrigger::IdleTimeout).await;
            }
            IdleOutcome::Fired(IdleTarget::PowerOff) => {
                match tokio::time::timeout(POWER_OFF_TIMEOUT, self.power_off.power_off_monitors())
                    .await
                {
                    Ok(Ok(())) => info!("saola-session: powered off monitors after idle timeout"),
                    // Severity rule 3 (spurious lock is the *worst* thing on
                    // this module's plate, and a failed power-off is not
                    // even that): log and otherwise ignore, per Architecture.
                    Ok(Err(err)) => warn!(
                        error = %err,
                        "saola-session: failed to power off monitors after idle timeout"
                    ),
                    Err(_) => warn!(
                        timeout_s = POWER_OFF_TIMEOUT.as_secs(),
                        "saola-session: 'niri msg action power-off-monitors' timed out"
                    ),
                }
            }
        }
    }
}

// ============================================================================
// The Wayland side. Not unit-tested (see module doc comment) — exercised
// live in a nested niri.
// ============================================================================

/// A Wayland connect/bind failure. Distinct from a *later* dispatch failure
/// (handled inline in [`wayland_thread_main`]) — this is specifically "we
/// never even got the notifications registered".
#[derive(Debug)]
struct IdleConnectError(String);

impl IdleConnectError {
    fn new(message: impl Into<String>) -> Self {
        IdleConnectError(message.into())
    }
}

impl fmt::Display for IdleConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `State` type for every `Dispatch` impl below. Deliberately tiny: it
/// holds only the channel back to [`run`]'s async task. Everything else
/// (the registry's global list, the notification proxies themselves) lives
/// as local variables in [`connect_and_register`]/[`wayland_thread_main`],
/// not as fields here — this struct's only job is being the thing `Dispatch`
/// is implemented on.
struct DispatchState {
    tx: mpsc::UnboundedSender<IdleEvent>,
}

/// `wl_registry`'s events (`global`/`global_remove`) are what
/// `registry_queue_init` needs a `Dispatch` impl to exist for — but we only
/// need the *initial* snapshot it hands back directly (this daemon does not
/// track seats or the notifier coming and going at runtime; Stage 5's scope
/// is "bind once at startup", not live hotplug), so the body does nothing.
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for DispatchState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

// `wl_seat` emits `capabilities`/`name` events we have no use for (we only
// need *a* seat object to hand to `get_idle_notification`) — `ignore` rather
// than the panicking variant of `delegate_noop!`, since a real seat will
// send these and doing nothing with them is expected, not a bug.
delegate_noop!(DispatchState: ignore WlSeat);

// `ext_idle_notifier_v1` has no events at all per the protocol XML (only
// requests) — `ignore` here is future-proofing against a hypothetical
// version bump adding one, not a response to anything this version sends.
delegate_noop!(DispatchState: ignore ExtIdleNotifierV1);

/// The one `Dispatch` impl that actually does something: `idled`/`resumed`
/// on a specific notification, tagged by which [`IdleTarget`] it was created
/// for (the `UserData` type parameter — see `get_idle_notification`'s call
/// site in [`connect_and_register`]).
impl Dispatch<ExtIdleNotificationV1, IdleTarget> for DispatchState {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        target: &IdleTarget,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let idle_event = match event {
            ext_idle_notification_v1::Event::Idled => IdleEvent::Idled(*target),
            ext_idle_notification_v1::Event::Resumed => IdleEvent::Resumed(*target),
            // Defensive: a future protocol version could add a variant we
            // don't know about yet. Dropping it is safe — the worst case is
            // this module missing an idle/resume edge it didn't understand,
            // never a panic.
            _ => return,
        };
        // An error here means `run`'s receiver is gone, i.e. the async side
        // already returned (shutdown, or an earlier fatal condition). There
        // is nothing left to notify; the send is simply dropped, not logged
        // (this can fire once per remaining idle/resume edge until process
        // exit, and would be noise, not signal).
        let _ = state.tx.send(idle_event);
    }
}

/// Connects to Wayland, binds `ext_idle_notifier_v1` and a `wl_seat`, and
/// registers one `ext_idle_notification_v1` per configured target. Returns
/// the live event queue plus the notification proxies — the caller must keep
/// both alive for the rest of the thread's life (dropping a proxy value does
/// not destroy the compositor-side object, but there is no reason to invite
/// confusion by letting bindings go out of scope early).
fn connect_and_register(
    idle: &IdleConfig,
    state: &mut DispatchState,
) -> Result<(EventQueue<DispatchState>, Vec<ExtIdleNotificationV1>), IdleConnectError> {
    let connection = Connection::connect_to_env()
        .map_err(|err| IdleConnectError::new(format!("Wayland connect failed: {err}")))?;

    let (globals, mut event_queue) = registry_queue_init::<DispatchState>(&connection)
        .map_err(|err| IdleConnectError::new(format!("Wayland registry init failed: {err}")))?;
    let qh = event_queue.handle();

    // `1..=Interface::version` rather than a hardcoded upper literal: `bind`
    // panics if the requested max exceeds the proxy type's own known max
    // version (`GlobalList::bind`'s doc: "a compile-time programmer error").
    // Deriving the end of the range from the interface itself instead of a
    // number we typed makes that panic structurally unreachable — the crate
    // no-panic rule extends to code that only *looks* infallible.
    let notifier: ExtIdleNotifierV1 = globals
        .bind(&qh, 1..=ExtIdleNotifierV1::interface().version, ())
        .map_err(|err| {
            IdleConnectError::new(format!(
                "ext-idle-notify-v1 is not available from this compositor: {err}"
            ))
        })?;
    let seat: WlSeat = globals
        .bind(&qh, 1..=WlSeat::interface().version, ())
        .map_err(|err| IdleConnectError::new(format!("no wl_seat is available: {err}")))?;

    let mut notifications = Vec::new();
    if let Some(timeout) = idle.lock_after {
        notifications.push(notifier.get_idle_notification(
            timeout_millis(timeout),
            &seat,
            &qh,
            IdleTarget::Lock,
        ));
        info!(
            seconds = timeout.as_secs(),
            "saola-session: idle-lock notification registered"
        );
    }
    if let Some(timeout) = idle.power_off_after {
        notifications.push(notifier.get_idle_notification(
            timeout_millis(timeout),
            &seat,
            &qh,
            IdleTarget::PowerOff,
        ));
        info!(
            seconds = timeout.as_secs(),
            "saola-session: idle-power-off notification registered"
        );
    }

    // A roundtrip here means any protocol errors from the two requests above
    // (a malformed timeout, say) surface now, from `connect_and_register`'s
    // `Result`, rather than silently later from inside the dispatch loop.
    event_queue
        .roundtrip(state)
        .map_err(|err| IdleConnectError::new(format!("initial Wayland roundtrip failed: {err}")))?;

    Ok((event_queue, notifications))
}

/// The dedicated OS thread's entry point (see the module doc comment's
/// "synchronous-Wayland-to-async bridge" section for why this is a plain
/// `std::thread` and not a tokio task). Connects, registers the configured
/// notifications, reports readiness once via `ready_tx`, then dispatches
/// forever — translating `idled`/`resumed` into [`IdleEvent`]s on `event_tx`
/// until the connection dies.
fn wayland_thread_main(
    idle: IdleConfig,
    event_tx: mpsc::UnboundedSender<IdleEvent>,
    ready_tx: oneshot::Sender<Result<(), IdleConnectError>>,
) {
    let mut state = DispatchState {
        tx: event_tx.clone(),
    };

    // E-4: retry the *startup* connect with bounded backoff rather than
    // dying on the first failure — see `STARTUP_CONNECT_ATTEMPTS`'s doc
    // comment for why a failure here is usually a boot-order race, not a
    // real absence of a compositor.
    let mut connected = None;
    let mut last_err = None;
    for attempt in 1..=STARTUP_CONNECT_ATTEMPTS {
        match connect_and_register(&idle, &mut state) {
            Ok(result) => {
                connected = Some(result);
                break;
            }
            Err(err) => {
                warn!(
                    attempt,
                    attempts = STARTUP_CONNECT_ATTEMPTS,
                    error = %err,
                    "saola-session: Wayland connect failed at startup — retrying (a lost boot \
                     race against the compositor is expected occasionally; a persistent failure \
                     past all attempts is not)"
                );
                last_err = Some(err);
                if attempt < STARTUP_CONNECT_ATTEMPTS {
                    thread::sleep(STARTUP_CONNECT_BACKOFF);
                }
            }
        }
    }

    let (mut event_queue, _notifications) = match connected {
        Some(connected) => connected,
        None => {
            // Every attempt failed. `last_err` is always `Some` here (the
            // loop only exits via `break` on success or by running out of
            // attempts, and every failed attempt sets it) — `unwrap_or_else`
            // rather than `unwrap` regardless, per the no-panic rule, so a
            // future refactor that breaks that invariant degrades to a
            // slightly less specific error message instead of a panic.
            let err = last_err.unwrap_or_else(|| {
                IdleConnectError::new("Wayland connect failed at startup with no recorded error")
            });
            // The async side is waiting on `ready_tx`; if it has already
            // given up (e.g. shutdown raced startup) the send fails and
            // there is nothing left to do either way.
            let _ = ready_tx.send(Err(err));
            return;
        }
    };

    if ready_tx.send(Ok(())).is_err() {
        info!(
            "saola-session: idle module's async side is already gone (shutdown raced Wayland \
             startup) — the Wayland thread has nothing left to report to and is exiting"
        );
        return;
    }

    info!("saola-session: idle module connected to Wayland; dispatching");
    loop {
        match event_queue.blocking_dispatch(&mut state) {
            Ok(_) => {}
            Err(err) => {
                // The connection died (compositor exit, socket error). This
                // is the silent-absence failure `CLAUDE.md` calls worse than
                // a crash if left unnoticed: idle policy would simply never
                // fire again. `event_tx`'s drop (when this function returns)
                // closes the channel, which `run`'s receiver observes as
                // `None` and treats as fatal — see `run`'s doc comment.
                error!(
                    error = %err,
                    "saola-session: Wayland dispatch failed — idle policy is no longer live"
                );
                return;
            }
        }
    }
}

/// The module's task, owned by `main.rs`'s `JoinSet`.
///
/// `inhibit_active` is Stage 6's plug-in point: a
/// [`watch::Receiver<bool>`] whose matching [`watch::Sender<bool>`]
/// `main.rs` already constructs (defaulting to `false` — the channel's
/// initial value — so "no inhibit" is the state until something ever sends
/// otherwise, exactly the "defaulting to no-inhibit" the task requires).
/// Nothing sends on it yet; Stage 6's `inhibit.rs` is the first real sender.
/// If that sender is ever dropped without this task also having stopped (the
/// stub holding it exits, or a future `inhibit.rs` task dies), `changed()`
/// starts returning `Err` immediately and forever — this loop stops polling
/// that branch after the first such `Err` (see `inhibit_closed` below)
/// rather than spinning on it, and idle policy keeps working with whatever
/// inhibit state was last observed (defaulting to `false` if none ever
/// arrived).
///
/// This task is **not** what joins the Wayland thread cleanly — see the
/// module doc comment. Returning here (on shutdown, or on a fatal Wayland
/// error) is what lets `main.rs`'s `JoinSet` observe this tokio task as
/// done; the underlying OS thread is intentionally left to be reclaimed at
/// process exit, which carries no exposure risk (Architecture's severity
/// order is about locking/suspend, not about how promptly a Wayland
/// dispatch thread's kernel-level `read()` unblocks).
pub async fn run(
    idle: IdleConfig,
    locker: SessionLocker,
    mut inhibit_active: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) {
    if idle.lock_after.is_none() && idle.power_off_after.is_none() {
        info!(
            "saola-session: idle policy disabled (no lock-after-secs or power-off-after-secs \
             in session.toml) — not opening a Wayland connection"
        );
        let _ = shutdown.changed().await;
        return;
    }

    let executor = IdleExecutor::new(locker, Arc::new(NiriPowerOff));
    let mut policy = IdlePolicy::new(&idle);

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = oneshot::channel();

    let spawn_result = thread::Builder::new()
        .name("saola-idle-wl".to_string())
        .spawn(move || wayland_thread_main(idle, event_tx, ready_tx));

    if let Err(err) = spawn_result {
        error!(
            error = %err,
            "saola-session: could not start the Wayland dispatch thread — idle policy cannot run"
        );
        return;
    }

    match ready_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            error!(error = %err, "saola-session: idle module failed to connect to Wayland");
            return;
        }
        Err(_) => {
            error!("saola-session: the Wayland dispatch thread ended before reporting readiness");
            return;
        }
    }

    // See this function's doc comment: once the inhibit-active channel's
    // sender is gone for good, stop polling it rather than busy-looping on
    // the `Err` `changed()` returns forever afterward.
    let mut inhibit_closed = false;

    loop {
        tokio::select! {
            // Shutdown first, always — same reasoning as `sleep.rs`'s
            // `biased` select: an operator-requested stop should not wait
            // behind whatever idle/resume event happens to be ready.
            biased;

            _ = shutdown.changed() => {
                info!("saola-session: idle module stopping");
                break;
            }

            changed = inhibit_active.changed(), if !inhibit_closed => {
                match changed {
                    Ok(()) => {
                        let active = *inhibit_active.borrow();
                        let outcome = policy.handle(IdleEvent::InhibitChanged(active));
                        executor.execute(outcome).await;
                    }
                    Err(_) => {
                        inhibit_closed = true;
                        debug!(
                            "saola-session: inhibit-active channel's sender is gone — idle \
                             policy continues with the last-known inhibit state (Stage 6 not \
                             wired yet, or its task stopped)"
                        );
                    }
                }
            }

            event = event_rx.recv() => {
                match event {
                    Some(event) => {
                        let outcome = policy.handle(event);
                        executor.execute(outcome).await;
                    }
                    None => {
                        // The Wayland thread ended (see `wayland_thread_main`'s
                        // dispatch-error path) and dropped its sender. Same
                        // fatal treatment `sleep.rs` gives a dead signal
                        // stream: return rather than strand a half-alive
                        // daemon that looks running but will never lock or
                        // power off on idle again.
                        error!(
                            "saola-session: the Wayland event channel closed — stopping so the \
                             daemon is restarted rather than left half-alive"
                        );
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::modules::sleep::{LockerSpawner, Logind, LogindError, SpawnError};

    // ------------------------------------------------------------------
    // IdlePolicy: the pure state machine. No async, no traits, no clock —
    // straight-line assertions against `handle`'s return value.
    // ------------------------------------------------------------------

    // The machine only cares whether each timeout is `Some` (armed) or
    // `None` (disabled) — the actual duration lives compositor-side — so
    // the helper takes seconds for the tests' readability.
    fn policy(lock_seconds: Option<u64>, power_off_seconds: Option<u64>) -> IdlePolicy {
        IdlePolicy::new(&IdleConfig {
            lock_after: lock_seconds.map(Duration::from_secs),
            power_off_after: power_off_seconds.map(Duration::from_secs),
        })
    }

    #[test]
    fn each_timeout_fires_its_action_once() {
        let mut p = policy(Some(5), Some(10));

        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::Fired(IdleTarget::Lock)
        );
        // A duplicate `Idled` without an intervening `Resumed` is a no-op —
        // it already fired for this idle period.
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::NoAction
        );

        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::PowerOff)),
            IdleOutcome::Fired(IdleTarget::PowerOff)
        );
    }

    #[test]
    fn resume_rearms_and_the_next_idle_fires_again() {
        let mut p = policy(Some(5), None);

        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::Fired(IdleTarget::Lock)
        );
        assert_eq!(
            p.handle(IdleEvent::Resumed(IdleTarget::Lock)),
            IdleOutcome::Rearmed(IdleTarget::Lock)
        );
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::Fired(IdleTarget::Lock)
        );
    }

    #[test]
    fn disabled_actions_never_fire() {
        let mut p = policy(Some(5), None);

        // power-off was never configured — Idled/Resumed for it are both
        // no-ops, regardless of how many times they arrive.
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::PowerOff)),
            IdleOutcome::NoAction
        );
        assert_eq!(
            p.handle(IdleEvent::Resumed(IdleTarget::PowerOff)),
            IdleOutcome::NoAction
        );

        // ...while the enabled target is unaffected.
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::Fired(IdleTarget::Lock)
        );
    }

    #[test]
    fn inhibit_suppresses_a_fresh_idle_period_but_not_an_already_fired_one() {
        let mut p = policy(Some(5), None);

        // Fires normally before any inhibit.
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::Fired(IdleTarget::Lock)
        );
        // Turning inhibit on afterward does not retroactively cancel that —
        // there is no cancellation path at all (Architecture: "never cancels
        // an in-flight lock"), only a flag consulted on the *next* Idled.
        assert_eq!(
            p.handle(IdleEvent::InhibitChanged(true)),
            IdleOutcome::InhibitChanged(true)
        );

        p.handle(IdleEvent::Resumed(IdleTarget::Lock));

        // Now a fresh idle period arrives while inhibited — suppressed, not
        // fired. What happens when the inhibit *clears* on top of this is
        // Stage 7's finding F-1's fix, covered by the dedicated tests below
        // rather than folded into this one.
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::Suppressed(IdleTarget::Lock)
        );
    }

    // ---------------------------------------------------------------- F-1:
    // an inhibit clearing must re-fire whatever it suppressed, because the
    // only alternative trigger — a real `Resumed` — may never come (the
    // "walk away during a film, it ends, nobody comes back" scenario).

    #[test]
    fn a_suppressed_target_fires_when_the_inhibit_clears_with_no_resumed() {
        let mut p = policy(Some(5), None);
        p.handle(IdleEvent::InhibitChanged(true));
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::Suppressed(IdleTarget::Lock)
        );

        // No `Resumed` anywhere in this test — modelling a user who never
        // comes back. The lock that was deferred fires now.
        assert_eq!(
            p.handle(IdleEvent::InhibitChanged(false)),
            IdleOutcome::Multiple(vec![
                IdleOutcome::InhibitChanged(false),
                IdleOutcome::Fired(IdleTarget::Lock),
            ])
        );

        // And the arm really did move to `Fired`, not stay `Suppressed`: a
        // further `Idled` with no intervening `Resumed` is the ordinary
        // duplicate no-op, not a second suppress-or-fire decision.
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::NoAction
        );
    }

    #[test]
    fn both_suppressed_targets_fire_independently_when_the_inhibit_clears() {
        let mut p = policy(Some(5), Some(10));
        p.handle(IdleEvent::InhibitChanged(true));
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::Suppressed(IdleTarget::Lock)
        );
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::PowerOff)),
            IdleOutcome::Suppressed(IdleTarget::PowerOff)
        );

        assert_eq!(
            p.handle(IdleEvent::InhibitChanged(false)),
            IdleOutcome::Multiple(vec![
                IdleOutcome::InhibitChanged(false),
                IdleOutcome::Fired(IdleTarget::Lock),
                IdleOutcome::Fired(IdleTarget::PowerOff),
            ])
        );
    }

    #[test]
    fn inhibit_clearing_with_nothing_suppressed_is_an_ordinary_flag_flip() {
        let mut p = policy(Some(5), None);
        p.handle(IdleEvent::InhibitChanged(true));
        // Nothing ever went `Idled` while active, so there is nothing owed.
        assert_eq!(
            p.handle(IdleEvent::InhibitChanged(false)),
            IdleOutcome::InhibitChanged(false)
        );
    }

    #[test]
    fn a_resumed_before_the_inhibit_clears_prevents_a_double_fire() {
        let mut p = policy(Some(5), None);
        p.handle(IdleEvent::InhibitChanged(true));
        p.handle(IdleEvent::Idled(IdleTarget::Lock)); // -> Suppressed

        // Real activity arrives *before* the inhibit clears — the ordinary
        // `Resumed` path re-arms it, same as if there had been no inhibit.
        assert_eq!(
            p.handle(IdleEvent::Resumed(IdleTarget::Lock)),
            IdleOutcome::Rearmed(IdleTarget::Lock)
        );

        // The target is `Armed`, not `Suppressed`, so the inhibit clearing
        // afterward must not also fire it — that would be a double fire for
        // one idle period.
        assert_eq!(
            p.handle(IdleEvent::InhibitChanged(false)),
            IdleOutcome::InhibitChanged(false)
        );
    }

    #[test]
    fn inhibit_is_independent_per_target() {
        let mut p = policy(Some(5), Some(10));
        p.handle(IdleEvent::InhibitChanged(true));

        // Both targets are suppressed while inhibited — the flag is global,
        // not per-target, matching Architecture (a single ScreenSaver
        // inhibit suppresses "new idle actions", not just one of the two).
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::Suppressed(IdleTarget::Lock)
        );
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::PowerOff)),
            IdleOutcome::Suppressed(IdleTarget::PowerOff)
        );
    }

    #[test]
    fn both_disabled_means_every_event_is_a_no_op() {
        let mut p = policy(None, None);

        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::Lock)),
            IdleOutcome::NoAction
        );
        assert_eq!(
            p.handle(IdleEvent::Idled(IdleTarget::PowerOff)),
            IdleOutcome::NoAction
        );
    }

    // ------------------------------------------------------------------
    // IdleExecutor: outcome → action, via fakes. Same journal-based
    // pattern `sleep.rs`'s tests use, so the two modules' test styles read
    // the same way.
    // ------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Step {
        LockedHintRead,
        LockerSpawned,
        PowerOffRan,
        PowerOffFailed,
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
    }

    /// A `Logind` that always reports "unlocked" — enough for
    /// `SessionLocker::lock_if_needed` to take the spawn path every time,
    /// which is all these tests need to observe.
    struct AlwaysUnlocked {
        journal: Journal,
        reads: AtomicUsize,
    }

    impl Logind for AlwaysUnlocked {
        fn acquire_delay_inhibitor(
            &self,
        ) -> BoxFuture<'_, Result<crate::modules::sleep::Inhibitor, LogindError>> {
            // Never called by `lock_if_needed`; only here to satisfy the
            // trait.
            Box::pin(async { Err(LogindError::new("not used by this fake")) })
        }

        fn locked_hint(&self) -> BoxFuture<'_, Result<bool, LogindError>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.journal.push(Step::LockedHintRead);
            Box::pin(async { Ok(false) })
        }
    }

    struct FakeSpawner {
        journal: Journal,
    }

    impl LockerSpawner for FakeSpawner {
        fn spawn_locker(&self) -> BoxFuture<'_, Result<(), SpawnError>> {
            self.journal.push(Step::LockerSpawned);
            Box::pin(async { Ok(()) })
        }
    }

    struct FakePowerOff {
        journal: Journal,
        fails: bool,
    }

    impl PowerOffCommand for FakePowerOff {
        fn power_off_monitors(&self) -> BoxFuture<'_, Result<(), PowerOffError>> {
            let journal = self.journal.clone();
            let fails = self.fails;
            Box::pin(async move {
                if fails {
                    journal.push(Step::PowerOffFailed);
                    Err(PowerOffError::new("fake: niri msg failed"))
                } else {
                    journal.push(Step::PowerOffRan);
                    Ok(())
                }
            })
        }
    }

    fn executor(power_off_fails: bool) -> (IdleExecutor, Journal) {
        let journal = Journal::default();
        let logind: Arc<dyn Logind> = Arc::new(AlwaysUnlocked {
            journal: journal.clone(),
            reads: AtomicUsize::new(0),
        });
        let spawner: Arc<dyn LockerSpawner> = Arc::new(FakeSpawner {
            journal: journal.clone(),
        });
        let locker = SessionLocker::new(logind, spawner);
        let power_off: Arc<dyn PowerOffCommand> = Arc::new(FakePowerOff {
            journal: journal.clone(),
            fails: power_off_fails,
        });
        (IdleExecutor::new(locker, power_off), journal)
    }

    #[tokio::test]
    async fn fired_lock_spawns_the_locker() {
        let (executor, journal) = executor(false);
        executor.execute(IdleOutcome::Fired(IdleTarget::Lock)).await;
        assert_eq!(
            journal.steps(),
            vec![Step::LockedHintRead, Step::LockerSpawned]
        );
    }

    #[tokio::test]
    async fn fired_power_off_runs_the_command() {
        let (executor, journal) = executor(false);
        executor
            .execute(IdleOutcome::Fired(IdleTarget::PowerOff))
            .await;
        assert_eq!(journal.steps(), vec![Step::PowerOffRan]);
    }

    #[tokio::test]
    async fn failed_power_off_is_logged_and_otherwise_ignored() {
        // Severity rule 3: this must not panic, must not retry forever, and
        // must not stop the executor from being usable again afterward.
        let (executor, journal) = executor(true);
        executor
            .execute(IdleOutcome::Fired(IdleTarget::PowerOff))
            .await;
        assert_eq!(journal.steps(), vec![Step::PowerOffFailed]);
    }

    #[tokio::test]
    async fn multiple_outcome_executes_every_sub_outcome_in_order() {
        // F-1's executor-level half: `IdlePolicy::handle` never runs
        // actions itself, so this proves the `Multiple` outcome it can now
        // return actually reaches the locker, not just that the pure
        // machine computed the right `IdleOutcome` value.
        let (executor, journal) = executor(false);
        executor
            .execute(IdleOutcome::Multiple(vec![
                IdleOutcome::InhibitChanged(false),
                IdleOutcome::Fired(IdleTarget::Lock),
            ]))
            .await;
        assert_eq!(
            journal.steps(),
            vec![Step::LockedHintRead, Step::LockerSpawned]
        );
    }

    #[tokio::test]
    async fn no_action_and_suppressed_and_rearmed_and_inhibit_changed_do_nothing_observable() {
        let (executor, journal) = executor(false);
        executor.execute(IdleOutcome::NoAction).await;
        executor
            .execute(IdleOutcome::Suppressed(IdleTarget::Lock))
            .await;
        executor
            .execute(IdleOutcome::Rearmed(IdleTarget::PowerOff))
            .await;
        executor.execute(IdleOutcome::InhibitChanged(true)).await;
        assert_eq!(journal.steps(), Vec::new());
    }
}
