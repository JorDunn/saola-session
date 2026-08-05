//! `org.freedesktop.ScreenSaver` inhibit shim — Stage 6.
//!
//! Implements Architecture's third concern (`PLAN.md`): the freedesktop
//! ScreenSaver `Inhibit(app, reason) -> cookie` / `UnInhibit(cookie)` pair,
//! so apps that ask the desktop *the freedesktop way* to suppress idle
//! (Firefox video is the canonical case) actually do. Architecture is
//! explicit and this module never deviates from it: **inhibits gate idle
//! only, never the before-sleep lock** — see [`run`]'s doc comment for how
//! that is enforced structurally (this module never even holds a handle to
//! `sleep.rs`'s [`crate::modules::sleep::SessionLocker`]).
//!
//! # Name ownership is conditional — read this before anything else here
//!
//! Stage 2's research (`docs/SIGNALS.md` §4) found that **niri already
//! implements this exact shim itself** (`org.freedesktop.ScreenSaver`, PID
//! confirmed to be niri, not a portal) and already wires its
//! `Inhibit`/`UnInhibit` into its own `ext-idle-notify-v1` suppression
//! *inside the compositor*, upstream of anything this daemon's `idle.rs`
//! (Stage 5) ever sees. On this machine, then, this module's job is almost
//! entirely to *notice that* and get out of the way. Jordan's resolution
//! (2026-08-03, recorded in Stage 5's handoff): build the **full** shim
//! anyway — cookie tracking, peer-vanish cleanup, the `watch::Sender<bool>`
//! feed — for a future non-niri compositor or as defense in depth, but make
//! ownership **conditional**: check at startup, and **never** attempt to
//! replace an existing owner. Concretely, [`ZbusNameClaimant::try_claim`]
//! requests the name with `RequestNameFlags::DoNotQueue` **alone** — no
//! `AllowReplacement`, no `ReplaceExisting`. That is load-bearing, not a
//! style choice: niri registers its own shim with
//! `RequestNameFlags::AllowReplacement` set (`docs/SIGNALS.md` §4's excerpt
//! of niri's `freedesktop_screensaver.rs`), which means a claim that also
//! set `ReplaceExisting` would **succeed** — taking the name away from niri
//! — and then silently break niri's own idle suppression for every other
//! app on the machine, the opposite of this module's purpose. Not
//! specifying `ReplaceExisting` means [`zbus::Connection::request_name_with_flags`]
//! returns `Err(zbus::Error::NameTaken)` whenever *anyone* already owns the
//! name, regardless of what flags they claimed it with — exactly the
//! "check, don't fight" behavior Stage 2/5 require.
//!
//! # Two halves, same shape as `sleep.rs`/`idle.rs`
//!
//! - **[`InhibitStore`]** is the pure cookie-bookkeeping machine —
//!   Architecture's "policy is a pure state machine over injected events"
//!   applied to this module's one piece of state (how many inhibits are
//!   active, and who holds each one). No D-Bus, no clock, no `Arc`. Every
//!   behavior Stage 6's task list names (grant/release, peer-vanish cleanup,
//!   double-`UnInhibit`, inhibit-state visibility) is a unit test against
//!   this type directly, same as `idle.rs`'s `IdlePolicy`.
//! - **[`NameClaimant`]** is this module's *only* small trait-behind-a-fake
//!   boundary (the `sleep.rs` `Logind`/`LockerSpawner` pattern, `idle.rs`'s
//!   `PowerOffCommand` sibling). It is deliberately the *one* thing behind a
//!   trait here: the D-Bus calls this module makes noise about
//!   (`Inhibit`/`UnInhibit`) are **inbound** — things D-Bus peers call on
//!   *us* — not outbound calls this module makes and could mock the
//!   response of. The only genuinely outbound, fallible, worth-faking
//!   D-Bus interaction here is the one-shot "try to claim the well-known
//!   name" at startup, so that is what gets the trait.
//!
//! Everything else — the actual `zbus::interface` impl, the `ObjectServer`
//! registration, and the `NameOwnerChanged` peer-vanish watcher — is
//! plumbing in the same sense `idle.rs`'s Wayland thread is: exercised live,
//! not unit-tested (see this module's tests for exactly where the line is
//! drawn, and the Stage 6 handoff for the live-check evidence).

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::message::Header;

use crate::modules::sleep::BoxFuture;

/// The well-known bus name **and** the D-Bus interface name — per the
/// freedesktop ScreenSaver spec these are literally the same string, which
/// is normal for a singleton service like this one (`docs/SIGNALS.md` §4
/// confirms niri registers under this exact name).
const SCREEN_SAVER_NAME: &str = "org.freedesktop.ScreenSaver";

/// The canonical object path every freedesktop-ScreenSaver-aware client
/// (Firefox included) addresses.
const SCREEN_SAVER_PATH: &str = "/org/freedesktop/ScreenSaver";

/// A second, legacy object path some older `xscreensaver`-compatible
/// clients still address. Cheap to also serve (docs/SIGNALS.md §4's excerpt
/// shows niri itself registers both paths for exactly this reason), so this
/// module mirrors it rather than risk being invisible to an older client
/// niri would have served.
const SCREEN_SAVER_LEGACY_PATH: &str = "/ScreenSaver";

/// The maximum length, in **characters** (not bytes), this daemon will
/// store or log for either the `app` or `reason` string of one `Inhibit`
/// call — Stage 7's finding D-1. `org.freedesktop.ScreenSaver` is an
/// unauthenticated session-bus service: any process on the user's session
/// bus may call it, with no policy check of any kind, so these two strings
/// are untrusted input in the same sense a network daemon's request body
/// would be. Truncating (not rejecting) keeps the interface's contract
/// simple — a caller still gets a valid cookie back — while bounding both
/// the memory a grant holds and the journal line logging it writes.
const MAX_FIELD_LEN: usize = 256;

/// The maximum number of live grants one bus peer may hold at once — D-1.
/// Past this, further `Inhibit` calls from that peer are refused (cookie
/// still returned, nothing stored) rather than accepted.
const MAX_GRANTS_PER_PEER: usize = 64;

/// The maximum number of live grants across *all* peers at once — D-1's
/// other half, bounding total memory even if the per-peer cap is spread
/// across many distinct peers (unusual for one user's own session bus, but
/// cheap to guard regardless).
const MAX_GRANTS_TOTAL: usize = 1024;

/// Truncates `s` to at most [`MAX_FIELD_LEN`] **characters**, returning a
/// borrowed slice (no allocation — used both for logging and, via
/// `.to_owned()` at the one call site that stores a [`Grant`], for
/// truncating what actually gets kept in memory).
///
/// Byte-slicing at a fixed offset (`&s[..MAX_FIELD_LEN]`) would panic if
/// that byte offset landed inside a multi-byte UTF-8 codepoint — this
/// crate's no-panic rule applies just as much to a string that arrived
/// straight off the session bus from an untrusted peer as it does to
/// anything else. `char_indices().nth(n)` finds the byte offset of the
/// `n`th character boundary, which by construction is always a valid place
/// to slice.
fn truncated(s: &str) -> &str {
    match s.char_indices().nth(MAX_FIELD_LEN) {
        Some((byte_index, _)) => &s[..byte_index],
        None => s,
    }
}

// ============================================================================
// InhibitStore — the pure cookie-bookkeeping machine. No D-Bus, no clock, no
// `Arc`/`Mutex`. Unit-tested directly, at the bottom of this file.
// ============================================================================

/// One granted cookie: which bus peer holds it (for peer-vanish cleanup) and
/// what it was for (kept only so the grant/release/vanish log lines can name
/// the app — Architecture never asks this module to police *which* apps may
/// inhibit, only to track who currently is).
#[derive(Debug, Clone)]
pub struct Grant {
    peer: String,
    pub app: String,
    pub reason: String,
}

/// Whether the active-cookie count just crossed zero, in either direction.
/// This is the thing [`run`] actually needs to know to decide whether to
/// `send` on the `watch::Sender<bool>` Stage 5's handoff describes — sending
/// on *every* grant/release (rather than only on a zero↔nonzero transition)
/// would still be harmless downstream (`idle::IdlePolicy::handle` treats a
/// same-valued `InhibitChanged` as a no-op in substance), but it would be
/// noise on the watch channel and in the journal for no benefit, so
/// [`InhibitStore`]'s methods compute this precisely rather than making
/// [`run`] re-derive it from a before/after `is_active()` comparison at
/// every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTransition {
    /// The active-cookie count is still zero, or still nonzero.
    Unchanged,
    /// Zero active cookies → one or more. The first inhibit just started.
    BecameActive,
    /// One or more active cookies → zero. The last inhibit just ended
    /// (whether by `UnInhibit` or by its peer vanishing).
    BecameInactive,
}

fn transition_for(was_active: bool, is_active: bool) -> ActiveTransition {
    match (was_active, is_active) {
        (false, true) => ActiveTransition::BecameActive,
        (true, false) => ActiveTransition::BecameInactive,
        _ => ActiveTransition::Unchanged,
    }
}

/// Whether an `Inhibit` call was actually recorded, or refused because it
/// would exceed one of Stage 7's finding D-1's caps.
///
/// A refused request still gets a cookie back from the D-Bus method (see
/// the interface impl below) — the freedesktop interface has no error
/// return, and a peer that got a D-Bus error where it expected a `u32`
/// would likely handle it worse than a harmless cookie. But no [`Grant`] is
/// stored, so the inhibit has no suppressing effect — the *safe* direction
/// under Architecture's severity order: idle actions keep firing rather
/// than a buggy or hostile peer being able to wedge this daemon's memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Granted,
    RefusedPerPeerCap,
    RefusedTotalCap,
}

/// The pure state behind `Inhibit`/`UnInhibit`: a cookie → grant map and a
/// monotonically-increasing cookie counter. Mirrors `idle.rs`'s `IdlePolicy`
/// in spirit — no I/O, `handle`-shaped methods, tests drive it directly.
#[derive(Debug, Default)]
pub struct InhibitStore {
    next_cookie: u32,
    grants: HashMap<u32, Grant>,
}

impl InhibitStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many inhibits are currently active. Exposed mainly for logging —
    /// [`Self::is_active`] is what callers should branch on.
    pub fn active_count(&self) -> usize {
        self.grants.len()
    }

    pub fn is_active(&self) -> bool {
        !self.grants.is_empty()
    }

    /// `Inhibit(application_name, reason_for_inhibit) -> cookie`.
    ///
    /// Cookie allocation is `wrapping_add`, not a plain `+= 1`: this crate's
    /// no-panic rule means an arithmetic overflow (impossible in practice —
    /// it would take four billion concurrent, never-released inhibits from
    /// one session's apps — but not impossible in *type*) must saturate or
    /// wrap, never panic. Wrapping back to a cookie that, in that same
    /// impossible scenario, happened to still be outstanding would silently
    /// reuse it (`HashMap::insert` overwrites) — ruled out of scope by
    /// Stage 7's review (finding I-2): a wrap this way is either preceded by
    /// D-1's `MAX_GRANTS_TOTAL` OOM-preventing refusal (the concurrent case)
    /// or requires days of sustained `Inhibit`/`UnInhibit` traffic (the
    /// sequential case), and even then the outcome is safe by direction — a
    /// collided cookie makes an honest app's inhibit silently stop working,
    /// which is severity 3, never severity 1.
    ///
    /// Stage 7's finding D-1: both `app` and `reason` are truncated to
    /// [`MAX_FIELD_LEN`] characters before being stored (bounding memory and
    /// the log line the caller writes), and the grant is refused — cookie
    /// still allocated and returned, nothing stored — once either
    /// [`MAX_GRANTS_PER_PEER`] or [`MAX_GRANTS_TOTAL`] is hit. See
    /// [`Admission`]'s doc comment for why refusing rather than erroring is
    /// the right shape for this interface, and why "refused" is the safe
    /// direction.
    pub fn inhibit(
        &mut self,
        peer: impl Into<String>,
        app: impl Into<String>,
        reason: impl Into<String>,
    ) -> (u32, ActiveTransition, Admission) {
        let was_active = self.is_active();
        let cookie = self.next_cookie;
        self.next_cookie = self.next_cookie.wrapping_add(1);

        let peer = peer.into();
        let app = app.into();
        let reason = reason.into();
        let app = truncated(&app).to_owned();
        let reason = truncated(&reason).to_owned();

        if self.grants.len() >= MAX_GRANTS_TOTAL {
            return (
                cookie,
                ActiveTransition::Unchanged,
                Admission::RefusedTotalCap,
            );
        }
        let peer_grants = self
            .grants
            .values()
            .filter(|grant| grant.peer == peer)
            .count();
        if peer_grants >= MAX_GRANTS_PER_PEER {
            return (
                cookie,
                ActiveTransition::Unchanged,
                Admission::RefusedPerPeerCap,
            );
        }

        self.grants.insert(cookie, Grant { peer, app, reason });
        (
            cookie,
            transition_for(was_active, self.is_active()),
            Admission::Granted,
        )
    }

    /// `UnInhibit(cookie)`. The `bool` says whether a grant actually existed
    /// under that cookie — **a double (or unknown-cookie) `UnInhibit` is not
    /// an error**, per the freedesktop spec's fire-and-forget cookie model
    /// and per Stage 6's own task list, which names this case explicitly. It
    /// resolves to `(false, ActiveTransition::Unchanged)`, nothing more.
    pub fn uninhibit(&mut self, cookie: u32) -> (bool, ActiveTransition) {
        let was_active = self.is_active();
        let found = self.grants.remove(&cookie).is_some();
        (found, transition_for(was_active, self.is_active()))
    }

    /// Drops every grant held by `peer` — called when [`run`]'s
    /// `NameOwnerChanged` watcher sees that peer's unique name vanish from
    /// the bus without an intervening `UnInhibit` (Architecture's task list:
    /// "apps crash without UnInhibiting — a leaked cookie is a permanent
    /// 'never lock on idle', severity rule 1"). Returns the dropped grants
    /// themselves (so the caller can name the crashed app and its stated
    /// reason in its log line — the whole point of keeping [`Grant::app`]
    /// and [`Grant::reason`] around at all) and whether the active flag
    /// changed.
    pub fn peer_vanished(&mut self, peer: &str) -> (Vec<Grant>, ActiveTransition) {
        let was_active = self.is_active();
        let vanished_cookies: Vec<u32> = self
            .grants
            .iter()
            .filter(|(_, grant)| grant.peer == peer)
            .map(|(&cookie, _)| cookie)
            .collect();
        let dropped = vanished_cookies
            .into_iter()
            .filter_map(|cookie| self.grants.remove(&cookie))
            .collect();
        (dropped, transition_for(was_active, self.is_active()))
    }
}

// ============================================================================
// NameClaimant — the one small trait-behind-a-fake boundary in this module.
// ============================================================================

/// Whether one conditional claim attempt ended with us owning the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// Nobody owned `org.freedesktop.ScreenSaver`; we are now its primary
    /// owner and should serve `Inhibit`/`UnInhibit` for real.
    Claimed,
    /// Someone else already owns it (niri, on every machine Stage 2 checked)
    /// — we did **not** attempt to replace them. Stay inert.
    AlreadyOwned,
}

/// A claim attempt failed for a reason other than "already owned" — a real
/// bus problem. Stringly-typed, same reasoning as `sleep.rs`'s
/// `LogindError`/`SpawnError`.
#[derive(Debug)]
pub struct ClaimError(String);

impl ClaimError {
    fn new(message: impl Into<String>) -> Self {
        ClaimError(message.into())
    }
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClaimError {}

/// The one external dependency this module abstracts behind a trait — same
/// one-method, boxed-future shape as `sleep.rs`'s `LockerSpawner` and
/// `idle.rs`'s `PowerOffCommand`, so [`decide_service_mode`] can be
/// unit-tested against a fake without a real bus connection.
pub trait NameClaimant: Send + Sync + 'static {
    /// One conditional claim attempt. Must **never** request
    /// `RequestNameFlags::ReplaceExisting` — see this module's doc comment
    /// for why that would silently break niri's own suppression.
    fn try_claim(&self) -> BoxFuture<'_, Result<ClaimOutcome, ClaimError>>;
}

/// The real [`NameClaimant`]: one `RequestName` call over `zbus`.
pub struct ZbusNameClaimant {
    connection: zbus::Connection,
}

impl ZbusNameClaimant {
    pub fn new(connection: zbus::Connection) -> Self {
        ZbusNameClaimant { connection }
    }
}

impl NameClaimant for ZbusNameClaimant {
    fn try_claim(&self) -> BoxFuture<'_, Result<ClaimOutcome, ClaimError>> {
        Box::pin(async move {
            // `DoNotQueue` and nothing else: no `AllowReplacement` (we are
            // not offering to give the name back up to a later claimant —
            // irrelevant here since we would be the only claimant that ever
            // matters, but leaving it unset is the conservative default) and
            // critically no `ReplaceExisting` (this module's doc comment:
            // the one flag that must never be set). Without `ReplaceExisting`,
            // zbus maps an already-owned name to `Err(zbus::Error::NameTaken)`
            // regardless of what flags the *current* owner claimed it with —
            // so niri's own `AllowReplacement` never matters to us.
            match self
                .connection
                .request_name_with_flags(SCREEN_SAVER_NAME, RequestNameFlags::DoNotQueue.into())
                .await
            {
                Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
                    Ok(ClaimOutcome::Claimed)
                }
                Ok(other) => Err(ClaimError::new(format!(
                    "unexpected RequestName reply with DoNotQueue-only flags: {other}"
                ))),
                Err(zbus::Error::NameTaken) => Ok(ClaimOutcome::AlreadyOwned),
                Err(err) => Err(ClaimError::new(format!("RequestName failed: {err}"))),
            }
        })
    }
}

/// Whether this run should actually serve `Inhibit`/`UnInhibit`, or stay
/// inert. Returned rather than only logged so [`decide_service_mode`] is
/// testable against its return value, same reasoning as every other
/// `*Outcome`/`*Mode` type in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceMode {
    Active,
    Inert,
}

/// Turns one [`NameClaimant::try_claim`] result into a [`ServiceMode`],
/// logging exactly what Stage 6's task requires: "if owned, log it and stay
/// inert; if unowned, claim ... and serve". A claim *error* (a genuine bus
/// problem, not "someone else owns it") resolves to `Inert` too — the same
/// severity-1 bias `sleep.rs`'s unreadable-`LockedHint` path takes: when we
/// cannot be sure a claim is safe, the conservative default is to not serve
/// rather than risk anything: this module's `Inert` path is never the
/// dangerous direction (Architecture: inhibits gate idle only; running
/// inert just means one fewer layer of idle-suppression on a machine where,
/// per Stage 2, niri already provides that layer natively).
async fn decide_service_mode(claimant: &dyn NameClaimant) -> ServiceMode {
    match claimant.try_claim().await {
        Ok(ClaimOutcome::Claimed) => {
            info!(
                name = SCREEN_SAVER_NAME,
                "saola-session: claimed the ScreenSaver name — serving Inhibit/UnInhibit"
            );
            ServiceMode::Active
        }
        Ok(ClaimOutcome::AlreadyOwned) => {
            info!(
                name = SCREEN_SAVER_NAME,
                "saola-session: {SCREEN_SAVER_NAME} is already owned (niri's own shim, on every \
                 machine Stage 2 checked) — staying inert rather than fighting over it; niri's \
                 own ext-idle-notify-v1 suppression already covers Inhibit/UnInhibit upstream of \
                 this daemon (docs/SIGNALS.md §4)"
            );
            ServiceMode::Inert
        }
        Err(err) => {
            warn!(
                error = %err,
                "saola-session: could not determine ScreenSaver name ownership — staying inert \
                 rather than risk a double claim"
            );
            ServiceMode::Inert
        }
    }
}

// ============================================================================
// The zbus interface — plumbing, not unit-tested (see module doc comment).
// Every method here does the minimum: call into `InhibitStore` under the
// lock, apply the transition, log, return.
// ============================================================================

/// State shared between the `Inhibit`/`UnInhibit` method handlers (each
/// inbound D-Bus call runs as its own concurrent invocation) and [`run`]'s
/// peer-vanish watcher task.
///
/// `std::sync::Mutex`, not `tokio::sync::Mutex`: every critical section here
/// is a handful of `HashMap` operations with no `.await` inside the guard —
/// the `watch::Sender::send` call always happens *after* the guard is
/// dropped (see [`Shared::apply`]) — so there is no `.await`-while-locked
/// hazard a blocking mutex would create. Same convention `sleep.rs` and
/// `idle.rs` follow for their own non-async state.
struct Shared {
    store: Mutex<InhibitStore>,
    active_tx: watch::Sender<bool>,
}

impl Shared {
    /// Locks [`Self::store`], recovering rather than panicking if it was
    /// poisoned (a prior handler panicking while holding it should never
    /// happen under this crate's no-panic rule, but a poisoned lock
    /// propagating as a fresh panic into the D-Bus dispatcher would be
    /// exactly the silent-looking-alive-but-broken failure `CLAUDE.md`
    /// warns against — recovering the (still perfectly usable) `HashMap`
    /// inside is strictly better than that).
    fn lock_store(&self) -> std::sync::MutexGuard<'_, InhibitStore> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sends on [`Self::active_tx`] only for a real zero↔nonzero transition,
    /// and logs the transition — this is the one place in the module that
    /// touches the watch channel, so every caller (both `Inhibit`/`UnInhibit`
    /// and the peer-vanish watcher) goes through it.
    fn apply(&self, transition: ActiveTransition) {
        match transition {
            ActiveTransition::BecameActive => {
                // `send` only errors when every receiver is gone, i.e.
                // `idle::run` has already stopped — nothing left to notify,
                // and `main.rs`'s shutdown-fatal handling for a returned
                // module task has already been tripped by that. Intentionally
                // ignored, same as every other `watch::Sender::send` in this
                // crate.
                let _ = self.active_tx.send(true);
                info!(
                    "saola-session: ScreenSaver inhibit became active — idle actions will be \
                     suppressed until it clears"
                );
            }
            ActiveTransition::BecameInactive => {
                let _ = self.active_tx.send(false);
                info!(
                    "saola-session: last ScreenSaver inhibit cleared — idle actions resume arming"
                );
            }
            ActiveTransition::Unchanged => {}
        }
    }
}

/// The `org.freedesktop.ScreenSaver` interface itself. Holds only the shared
/// state — no D-Bus connection, no object path — so the same value can (and
/// is, in [`run`]) be registered at both [`SCREEN_SAVER_PATH`] and
/// [`SCREEN_SAVER_LEGACY_PATH`] via two cheap `Arc` clones.
struct ScreenSaverInterface {
    shared: Arc<Shared>,
}

/// Method-name note (the teaching note `sleep.rs`'s `proxies` module has for
/// its own two `GetSessionBy...PID` overrides, restated here because this is
/// the mirror-image case): `zbus`'s `#[interface]` macro pascal-cases a Rust
/// method name to get the D-Bus member name it exposes. `inhibit` → `Inhibit`
/// and `un_inhibit` → `UnInhibit` both land exactly on the freedesktop
/// spec's real member names with **no** `#[zbus(name = "...")]` override
/// needed — verified against the macro's own `pascal_case` (splits on `_`,
/// capitalizes after each split), not assumed.
#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaverInterface {
    async fn inhibit(
        &self,
        application_name: &str,
        reason_for_inhibit: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> u32 {
        // The session bus always stamps a `sender` header on messages it
        // routes (this daemon only ever talks to a real message bus, never
        // a peer-to-peer connection), so this `else` branch is believed
        // unreachable in practice — Stage 7's finding L-4 is the ruling on
        // it, made at Stage 6's request. But `unwrap_or_default()` used to
        // sit here, and that converts "impossible" into "silently
        // degraded permanent": an empty peer name can never match a real
        // `NameOwnerChanged` peer, so a grant stored under one could only
        // ever be released by an explicit `UnInhibit` — a leaked-cookie
        // shape Architecture calls severity 1 if the caller then crashes.
        // Refusing to store it (rather than trusting a default) costs
        // three lines and removes the possibility entirely.
        let Some(peer) = header.sender().map(ToString::to_string) else {
            warn!(
                app = application_name,
                "saola-session: ScreenSaver Inhibit arrived with no sender header — refusing to \
                 track an untrackable grant (it could never be cleaned up if the caller crashed)"
            );
            // A cookie the caller may harmlessly `UnInhibit` later; nothing
            // is stored, so this inhibit has no suppressing effect — the
            // safe direction (idle actions keep firing).
            return 0;
        };

        let (cookie, transition, active_count, admission) = {
            let mut store = self.shared.lock_store();
            let (cookie, transition, admission) =
                store.inhibit(peer.clone(), application_name, reason_for_inhibit);
            (cookie, transition, store.active_count(), admission)
        };
        self.shared.apply(transition);

        // Logged truncated, not raw: Stage 7's finding D-1 bounds the
        // journal the same way it bounds the `HashMap` — an untrusted peer
        // that sends megabyte-sized strings must not get to write
        // megabytes to the journal per call just because logging happened
        // to use the un-truncated originals.
        let app = truncated(application_name);
        let reason = truncated(reason_for_inhibit);
        match admission {
            Admission::Granted => {
                info!(
                    app,
                    reason,
                    peer,
                    cookie,
                    active_count,
                    "saola-session: ScreenSaver Inhibit granted"
                );
            }
            Admission::RefusedPerPeerCap => {
                warn!(
                    app,
                    peer,
                    cookie,
                    cap = "per-peer",
                    "saola-session: ScreenSaver Inhibit refused — this peer already holds \
                     MAX_GRANTS_PER_PEER live grants (D-1); the cookie is valid to UnInhibit but \
                     has no suppressing effect"
                );
            }
            Admission::RefusedTotalCap => {
                warn!(
                    app,
                    peer,
                    cookie,
                    cap = "total",
                    "saola-session: ScreenSaver Inhibit refused — MAX_GRANTS_TOTAL live grants \
                     are already held across all peers (D-1); the cookie is valid to UnInhibit \
                     but has no suppressing effect"
                );
            }
        }
        cookie
    }

    async fn un_inhibit(&self, cookie: u32) {
        let (found, transition) = {
            let mut store = self.shared.lock_store();
            store.uninhibit(cookie)
        };
        self.shared.apply(transition);
        if found {
            debug!(
                cookie,
                "saola-session: ScreenSaver UnInhibit released a cookie"
            );
        } else {
            // Not a warning, let alone an error: an unknown/already-released
            // cookie is explicitly tolerated by the freedesktop cookie model
            // and by Stage 6's own task list ("double-UnInhibit").
            debug!(
                cookie,
                "saola-session: ScreenSaver UnInhibit for an unknown or already-released cookie \
                 — ignored, not an error"
            );
        }
    }
}

/// Polls one signal stream for its next item. Identical in shape and
/// reasoning to `sleep.rs`'s private `next_signal` — duplicated here rather
/// than made `pub(crate)` and shared, since it is four lines and pulling in
/// a cross-module dependency for it would cost more readability than it
/// saves. See `sleep.rs`'s copy for the full teaching note on why this
/// replaces `futures::StreamExt::next`.
async fn next_signal<S>(stream: &mut S) -> Option<S::Item>
where
    S: zbus::export::futures_core::Stream + Unpin,
{
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

/// Watches `org.freedesktop.DBus`'s `NameOwnerChanged` signal for a peer
/// that holds at least one grant dropping off the bus without calling
/// `UnInhibit` — Architecture's task list: "apps crash without
/// UnInhibiting; a leaked cookie is a permanent 'never lock on idle'".
///
/// Filtered server-side to `(2, "")` — "the third argument (`new_owner`) is
/// empty" — the exact shape `docs/SIGNALS.md` §4's excerpt of niri's own
/// `freedesktop_screensaver.rs` uses for this, so this task only wakes for
/// an actual bus disconnect, not every name change on the bus (which on a
/// busy session bus would be most of its traffic).
async fn watch_for_vanished_peers(
    dbus: zbus::fdo::DBusProxy<'_>,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut stream = match dbus.receive_name_owner_changed_with_args(&[(2, "")]).await {
        Ok(stream) => stream,
        Err(err) => {
            error!(
                error = %err,
                "saola-session: could not subscribe to NameOwnerChanged — a crashed inhibiting \
                 app's cookie will not be cleaned up automatically until the daemon restarts; \
                 Inhibit/UnInhibit keep working normally in the meantime"
            );
            let _ = shutdown.changed().await;
            return;
        }
    };

    loop {
        tokio::select! {
            // Same reasoning as `sleep.rs`/`idle.rs`'s `biased` selects: an
            // operator-requested stop must not wait behind whatever signal
            // happens to be ready.
            biased;

            _ = shutdown.changed() => {
                debug!("saola-session: inhibit peer-vanish watcher stopping");
                return;
            }

            signal = next_signal(&mut stream) => {
                match signal {
                    Some(signal) => match signal.args() {
                        Ok(args) => {
                            let peer = args.name.to_string();
                            let (dropped, transition) = {
                                let mut store = shared.lock_store();
                                store.peer_vanished(&peer)
                            };
                            if !dropped.is_empty() {
                                // Named per-grant so the journal shows which
                                // app(s) crashed and what each one said it
                                // was inhibiting for — `Grant::app`/`::reason`
                                // exist for exactly this line.
                                let grants: Vec<String> = dropped
                                    .iter()
                                    .map(|grant| format!("{} ({})", grant.app, grant.reason))
                                    .collect();
                                warn!(
                                    peer,
                                    grants = grants.join(", "),
                                    removed = dropped.len(),
                                    "saola-session: a ScreenSaver-inhibiting peer disappeared \
                                     from the bus without calling UnInhibit — dropped its \
                                     cookie(s) rather than leaving idle permanently suppressed"
                                );
                                shared.apply(transition);
                            }
                        }
                        Err(err) => warn!(
                            error = %err,
                            "saola-session: could not decode a NameOwnerChanged signal"
                        ),
                    },
                    None => {
                        // Stage 7's finding L-3: this is fatal to the whole
                        // process, not merely to this task. `watch_for_vanished_peers`
                        // is `.await`ed inline as the tail of `run` (see its
                        // call site), so returning from it returns from
                        // `run` — which `main.rs`'s `JoinSet` treats like
                        // any other module task ending early: the process
                        // exits non-zero and systemd restarts it. An
                        // earlier version of this comment claimed
                        // `Inhibit`/`UnInhibit` would keep working after
                        // this branch fires; they do not, because nothing
                        // does once the process is gone. The *behavior*
                        // (die and get restarted rather than silently drop
                        // peer-vanish cleanup — a leaked cookie is a
                        // permanent "never lock on idle", severity 1) is
                        // still the right one; only the words describing it
                        // were wrong.
                        error!(
                            "saola-session: the NameOwnerChanged signal stream ended — the D-Bus \
                             connection is gone; stopping so the daemon is restarted rather than \
                             left running with peer-vanish cookie cleanup silently disabled"
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// The module's task, owned by `main.rs`'s `JoinSet`. Replaces
/// `inhibit_module_stub` (Stage 5's handoff): takes the same `inhibit_tx`
/// Stage 5 already constructs and holds it for this task's entire lifetime,
/// sending `true`/`false` on it exactly on zero↔nonzero transitions of the
/// active-cookie count (via [`Shared::apply`]), and never touches
/// `sleep.rs` at all — there is no field, parameter, or import anywhere in
/// this module that reaches the sleep path, which is how "inhibits gate
/// idle only, never the before-sleep lock" (Architecture, binding) is
/// enforced structurally rather than by a runtime check someone could get
/// wrong later.
///
/// Fatal to the process in exactly one place, everywhere else degraded and
/// running: a D-Bus connect failure, a failed object registration, or a
/// failed name claim all degrade to "stay inert, hold `inhibit_tx` alive,
/// wait for shutdown" rather than returning early or panicking — same shape
/// as `sleep.rs`'s degraded logind mode, and for the same reason: this
/// module's worst *inert* failure (never suppressing an idle action) is
/// Architecture's *lowest* severity concern (idle-only), unlike `idle.rs`'s
/// Wayland connection, whose failure is fatal because that module has no
/// lower-severity fallback to degrade to. The one exception (Stage 7's
/// finding L-3) is [`watch_for_vanished_peers`]'s signal stream ending,
/// which *is* fatal — see its own `None` branch for why that one is
/// different from the degraded-and-inert paths above it.
pub async fn run(inhibit_tx: watch::Sender<bool>, mut shutdown: watch::Receiver<bool>) {
    let connection = match zbus::Connection::session().await {
        Ok(connection) => connection,
        Err(err) => {
            warn!(
                error = %err,
                "saola-session: no session bus connection — the ScreenSaver inhibit shim is \
                 inert for this run (idle policy runs exactly as if nothing ever calls Inhibit, \
                 which is the safe default)"
            );
            let _ = shutdown.changed().await;
            return;
        }
    };

    let shared = Arc::new(Shared {
        store: Mutex::new(InhibitStore::new()),
        active_tx: inhibit_tx,
    });

    // Registered *before* the conditional name claim below, regardless of
    // which way that claim goes — `zbus::Connection::request_name_with_flags`'s
    // own doc calls out setting up the `ObjectServer` first as the way to
    // avoid a window where an inbound call could be routed here and find
    // nothing registered yet.
    if let Err(err) = connection
        .object_server()
        .at(
            SCREEN_SAVER_PATH,
            ScreenSaverInterface {
                shared: Arc::clone(&shared),
            },
        )
        .await
    {
        error!(
            error = %err,
            "saola-session: could not register the ScreenSaver object at {SCREEN_SAVER_PATH} — \
             the inhibit shim is inert for this run"
        );
        let _ = shutdown.changed().await;
        return;
    }
    // The legacy alias is a compatibility nicety, not the primary path — a
    // failure here is worth a warning but must not take the whole shim
    // inert (unlike the canonical path above).
    if let Err(err) = connection
        .object_server()
        .at(
            SCREEN_SAVER_LEGACY_PATH,
            ScreenSaverInterface {
                shared: Arc::clone(&shared),
            },
        )
        .await
    {
        warn!(
            error = %err,
            "saola-session: could not register the legacy {SCREEN_SAVER_LEGACY_PATH} alias — \
             continuing with only {SCREEN_SAVER_PATH} served"
        );
    }

    let claimant = ZbusNameClaimant::new(connection.clone());
    match decide_service_mode(&claimant).await {
        ServiceMode::Inert => {
            // Hold `inhibit_tx` (inside `shared`, via `Arc`) alive and never
            // send on it — the channel's initial `false` is already
            // `idle::run`'s "no inhibit" default (Stage 5's handoff), so
            // doing nothing here is exactly correct, not a stopgap.
            let _ = shutdown.changed().await;
        }
        ServiceMode::Active => {
            match zbus::fdo::DBusProxy::new(&connection).await {
                Ok(dbus) => watch_for_vanished_peers(dbus, shared, shutdown).await,
                Err(err) => {
                    error!(
                        error = %err,
                        "saola-session: could not build the D-Bus proxy needed for peer-vanish \
                         cleanup — Inhibit/UnInhibit still work (the ObjectServer registration \
                         above is independent of this), but a crashed inhibiting app's cookie \
                         will leak until the daemon restarts"
                    );
                    // Still wait for shutdown rather than returning
                    // immediately: returning now would drop `connection`
                    // (a `#[must_use]` type specifically because dropping it
                    // closes the socket), which would silently stop serving
                    // Inhibit/UnInhibit too — far worse than the
                    // peer-vanish-cleanup gap this branch is already about.
                    let _ = shutdown.changed().await;
                }
            }
        }
    }

    info!("saola-session: inhibit module stopping");
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // InhibitStore: the pure cookie machine. No async, no traits, no D-Bus.
    // ------------------------------------------------------------------

    #[test]
    fn first_inhibit_becomes_active_and_grants_cookie_zero() {
        let mut store = InhibitStore::new();
        let (cookie, transition, _admission) = store.inhibit("peer.1", "firefox", "video playback");
        assert_eq!(cookie, 0);
        assert_eq!(transition, ActiveTransition::BecameActive);
        assert!(store.is_active());
        assert_eq!(store.active_count(), 1);
    }

    #[test]
    fn cookies_are_distinct_and_increasing() {
        let mut store = InhibitStore::new();
        let (first, _, _) = store.inhibit("peer.1", "app-a", "reason-a");
        let (second, transition, _admission) = store.inhibit("peer.2", "app-b", "reason-b");
        assert_ne!(first, second);
        assert_eq!(second, first + 1);
        // A second concurrent inhibit does not re-fire "became active" —
        // the count was already nonzero.
        assert_eq!(transition, ActiveTransition::Unchanged);
        assert_eq!(store.active_count(), 2);
    }

    #[test]
    fn releasing_the_last_cookie_becomes_inactive() {
        let mut store = InhibitStore::new();
        let (cookie, _, _) = store.inhibit("peer.1", "firefox", "video playback");

        let (found, transition) = store.uninhibit(cookie);
        assert!(found);
        assert_eq!(transition, ActiveTransition::BecameInactive);
        assert!(!store.is_active());
    }

    #[test]
    fn releasing_one_of_several_stays_active() {
        let mut store = InhibitStore::new();
        let (first, _, _) = store.inhibit("peer.1", "app-a", "reason-a");
        let (_second, _, _) = store.inhibit("peer.2", "app-b", "reason-b");

        let (found, transition) = store.uninhibit(first);
        assert!(found);
        assert_eq!(transition, ActiveTransition::Unchanged);
        assert!(store.is_active());
        assert_eq!(store.active_count(), 1);
    }

    /// Stage 6's task list names this explicitly: a double (or
    /// unknown-cookie) `UnInhibit` must not be treated as an error and must
    /// not affect the active flag.
    #[test]
    fn double_uninhibit_is_not_an_error_and_does_nothing() {
        let mut store = InhibitStore::new();
        let (cookie, _, _) = store.inhibit("peer.1", "firefox", "video playback");

        let (found_first, transition_first) = store.uninhibit(cookie);
        assert!(found_first);
        assert_eq!(transition_first, ActiveTransition::BecameInactive);

        let (found_second, transition_second) = store.uninhibit(cookie);
        assert!(!found_second);
        assert_eq!(transition_second, ActiveTransition::Unchanged);
    }

    #[test]
    fn uninhibit_of_a_cookie_that_never_existed_is_not_an_error() {
        let mut store = InhibitStore::new();
        let (found, transition) = store.uninhibit(999);
        assert!(!found);
        assert_eq!(transition, ActiveTransition::Unchanged);
    }

    /// The peer-vanish path: a crashed app's cookie(s) are dropped, and only
    /// that peer's cookies — a still-live peer's grant survives.
    #[test]
    fn peer_vanished_drops_only_that_peers_cookies() {
        let mut store = InhibitStore::new();
        let (crashed_cookie, _, _) = store.inhibit(":1.50", "crashed-app", "reason");
        let (survivor_cookie, _, _) = store.inhibit(":1.51", "still-running-app", "reason");

        let (dropped, transition) = store.peer_vanished(":1.50");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].app, "crashed-app");
        assert_eq!(transition, ActiveTransition::Unchanged);
        assert!(store.is_active());

        // The crashed peer's cookie is really gone (a redundant UnInhibit
        // for it, if the (now-dead) peer somehow still sent one, would be
        // the ordinary "unknown cookie" no-op case).
        let (found, _) = store.uninhibit(crashed_cookie);
        assert!(!found);

        // The survivor is untouched.
        let (found, transition) = store.uninhibit(survivor_cookie);
        assert!(found);
        assert_eq!(transition, ActiveTransition::BecameInactive);
    }

    /// A peer holding *every* active cookie vanishing is exactly what
    /// clears the inhibit — the `BecameInactive` transition Architecture's
    /// severity-1 framing cares about (a leaked cookie must not mean
    /// "idle-lock never fires again").
    #[test]
    fn last_peer_vanishing_becomes_inactive() {
        let mut store = InhibitStore::new();
        store.inhibit(":1.50", "crashed-app", "reason");

        let (dropped, transition) = store.peer_vanished(":1.50");
        assert_eq!(dropped.len(), 1);
        assert_eq!(transition, ActiveTransition::BecameInactive);
        assert!(!store.is_active());
    }

    /// A `NameOwnerChanged` for a peer that never held any cookie is a
    /// harmless no-op — most bus disconnects are not inhibit holders at all.
    #[test]
    fn vanishing_of_an_unrelated_peer_does_nothing() {
        let mut store = InhibitStore::new();
        store.inhibit(":1.50", "app", "reason");

        let (dropped, transition) = store.peer_vanished(":1.999");
        assert!(dropped.is_empty());
        assert_eq!(transition, ActiveTransition::Unchanged);
        assert!(store.is_active());
    }

    // ------------------------------------------------------------------
    // Stage 7's finding D-1: bounded strings and bounded grant counts.
    // `InhibitStore` is pure, so these are ordinary unit tests — no D-Bus,
    // no fake peer, no session bus needed to prove the caps hold.
    // ------------------------------------------------------------------

    #[test]
    fn truncated_leaves_a_short_string_untouched() {
        assert_eq!(truncated("firefox"), "firefox");
    }

    #[test]
    fn truncated_cuts_a_long_ascii_string_to_the_field_limit() {
        let long = "a".repeat(MAX_FIELD_LEN + 100);
        let cut = truncated(&long);
        assert_eq!(cut.chars().count(), MAX_FIELD_LEN);
    }

    /// The load-bearing case: a fixed *byte* offset would panic mid-codepoint
    /// on a multi-byte string. `truncated` must slice on a character
    /// boundary regardless of how many bytes each character takes.
    #[test]
    fn truncated_is_char_boundary_safe_on_multi_byte_input() {
        // Each "🔒" is 4 bytes; `MAX_FIELD_LEN` bytes would land inside one.
        let long = "🔒".repeat(MAX_FIELD_LEN + 10);
        let cut = truncated(&long);
        assert_eq!(cut.chars().count(), MAX_FIELD_LEN);
        // The real assertion: this must not have panicked, and the result
        // must be valid UTF-8 (guaranteed by `&str` itself, but the point
        // is that we got here at all rather than panicking on a
        // mid-codepoint slice).
        assert!(cut.chars().all(|c| c == '🔒'));
    }

    #[test]
    fn a_stored_grant_is_truncated_to_the_field_limit() {
        let mut store = InhibitStore::new();
        let long_app = "x".repeat(MAX_FIELD_LEN + 50);
        let (cookie, _, admission) = store.inhibit("peer.1", long_app, "reason");
        assert_eq!(admission, Admission::Granted);

        let (dropped, _) = store.peer_vanished("peer.1");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].app.chars().count(), MAX_FIELD_LEN);
        let _ = cookie;
    }

    #[test]
    fn a_peer_past_the_per_peer_cap_is_refused_but_still_gets_a_cookie() {
        let mut store = InhibitStore::new();
        for _ in 0..MAX_GRANTS_PER_PEER {
            let (_, _, admission) = store.inhibit("peer.1", "app", "reason");
            assert_eq!(admission, Admission::Granted);
        }
        assert_eq!(store.active_count(), MAX_GRANTS_PER_PEER);

        // One more from the same peer is refused...
        let (cookie, transition, admission) = store.inhibit("peer.1", "app", "reason");
        assert_eq!(admission, Admission::RefusedPerPeerCap);
        assert_eq!(transition, ActiveTransition::Unchanged);
        // ...but a cookie was still allocated (harmless to UnInhibit later)...
        let _ = cookie;
        // ...and, critically, nothing was actually stored: the count did not
        // grow past the cap.
        assert_eq!(store.active_count(), MAX_GRANTS_PER_PEER);

        // A *different* peer is unaffected by peer.1's cap.
        let (_, _, admission) = store.inhibit("peer.2", "app", "reason");
        assert_eq!(admission, Admission::Granted);
    }

    #[test]
    fn the_total_cap_is_refused_even_across_many_distinct_peers() {
        let mut store = InhibitStore::new();
        for i in 0..MAX_GRANTS_TOTAL {
            let peer = format!("peer.{i}");
            let (_, _, admission) = store.inhibit(peer, "app", "reason");
            assert_eq!(admission, Admission::Granted);
        }
        assert_eq!(store.active_count(), MAX_GRANTS_TOTAL);

        let (_, _, admission) = store.inhibit("one.peer.too.many", "app", "reason");
        assert_eq!(admission, Admission::RefusedTotalCap);
        assert_eq!(store.active_count(), MAX_GRANTS_TOTAL);
    }

    /// A capped peer's refused `Inhibit` must not suppress idle — the
    /// grant was never stored, so `is_active()`'s truth (and therefore
    /// whatever `idle.rs` observes through the watch channel) is exactly as
    /// if the call had never happened.
    #[test]
    fn a_capped_peer_cannot_suppress_idle() {
        let mut store = InhibitStore::new();
        for _ in 0..MAX_GRANTS_PER_PEER {
            store.inhibit("peer.1", "app", "reason");
        }
        let was_active = store.is_active();
        let (_, transition, admission) = store.inhibit("peer.1", "app", "reason");
        assert_eq!(admission, Admission::RefusedPerPeerCap);
        assert_eq!(transition, ActiveTransition::Unchanged);
        assert_eq!(store.is_active(), was_active);
    }

    // ------------------------------------------------------------------
    // Inhibit-state visibility: `Shared::apply` really does drive a
    // `watch::Receiver<bool>` the way `idle::run` consumes it — no D-Bus
    // involved, just `InhibitStore` + a real `tokio::sync::watch` channel.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn active_transitions_are_visible_on_the_watch_channel() {
        let (tx, mut rx) = watch::channel(false);
        let shared = Shared {
            store: Mutex::new(InhibitStore::new()),
            active_tx: tx,
        };
        assert!(!*rx.borrow());

        let (cookie, transition, _admission) = {
            let mut store = shared.lock_store();
            store.inhibit("peer.1", "firefox", "video playback")
        };
        shared.apply(transition);
        assert!(rx.has_changed().unwrap());
        rx.changed().await.unwrap();
        assert!(*rx.borrow());

        let (_, transition) = {
            let mut store = shared.lock_store();
            store.uninhibit(cookie)
        };
        shared.apply(transition);
        rx.changed().await.unwrap();
        assert!(!*rx.borrow());
    }

    /// A second concurrent inhibit must not re-send `true` — Stage 5's
    /// handoff asks for sends "on zero↔nonzero transitions", not on every
    /// grant. Proven by asserting `has_changed()` is false after the second
    /// grant (the receiver already observed the one and only `true`).
    #[tokio::test]
    async fn a_second_concurrent_inhibit_does_not_resend() {
        let (tx, mut rx) = watch::channel(false);
        let shared = Shared {
            store: Mutex::new(InhibitStore::new()),
            active_tx: tx,
        };

        let (_, t1, _) = {
            let mut store = shared.lock_store();
            store.inhibit("peer.1", "app-a", "reason-a")
        };
        shared.apply(t1);
        rx.changed().await.unwrap();
        assert!(*rx.borrow());

        let (_, t2, _) = {
            let mut store = shared.lock_store();
            store.inhibit("peer.2", "app-b", "reason-b")
        };
        shared.apply(t2);
        // `Unchanged` was returned, so `apply` never called `send` a second
        // time — the receiver has nothing new to observe.
        assert!(!rx.has_changed().unwrap());
    }

    // ------------------------------------------------------------------
    // NameClaimant / decide_service_mode: the trait-and-fake boundary.
    // ------------------------------------------------------------------

    struct FakeClaimant {
        result: Result<ClaimOutcome, &'static str>,
    }

    impl NameClaimant for FakeClaimant {
        fn try_claim(&self) -> BoxFuture<'_, Result<ClaimOutcome, ClaimError>> {
            let result = match self.result {
                Ok(outcome) => Ok(outcome),
                Err(msg) => Err(ClaimError::new(msg)),
            };
            Box::pin(async move { result })
        }
    }

    #[tokio::test]
    async fn unowned_name_is_claimed_and_serves() {
        let claimant = FakeClaimant {
            result: Ok(ClaimOutcome::Claimed),
        };
        assert_eq!(decide_service_mode(&claimant).await, ServiceMode::Active);
    }

    #[tokio::test]
    async fn already_owned_name_stays_inert() {
        let claimant = FakeClaimant {
            result: Ok(ClaimOutcome::AlreadyOwned),
        };
        assert_eq!(decide_service_mode(&claimant).await, ServiceMode::Inert);
    }

    /// A genuine claim-attempt error (not "someone else owns it") is also
    /// conservative-default-inert — never a reason to try harder and risk a
    /// double claim.
    #[tokio::test]
    async fn claim_error_stays_inert() {
        let claimant = FakeClaimant {
            result: Err("fake: bus hiccup"),
        };
        assert_eq!(decide_service_mode(&claimant).await, ServiceMode::Inert);
    }
}
