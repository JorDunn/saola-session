# Stage 4 handoff — before-sleep + lock-signal module (`src/modules/sleep.rs`)

Compressed state for Stage 5. Everything below is as-built and verified
(`cargo build && cargo clippy --all-targets -- -D warnings && cargo test`
exit 0; `cargo fmt --check` clean; 49 tests, 23 of them new here).

## What Stage 5 must reuse (read this section first)

**`modules::sleep::SessionLocker` is the crate's only locker-spawn path.**
`idle.rs` must call it, not spawn anything itself (Architecture: "one
implementation, not two").

```rust
#[derive(Clone)]                      // two Arc bumps; cheap to clone
pub struct SessionLocker { /* Arc<dyn Logind>, Arc<dyn LockerSpawner> */ }

impl SessionLocker {
    pub async fn lock_if_needed(&self, trigger: LockTrigger) -> LockAttempt;
}

pub enum LockTrigger { BeforeSleep, LogindLockSignal, IdleTimeout }
pub enum LockAttempt { AlreadyLocked, Spawned, SpawnFailed }
```

`lock_if_needed` = read `LockedHint` → if `true`, log and return
`AlreadyLocked` (never stacks lockers) → else spawn detached and return
`Spawned`/`SpawnFailed`. It logs everything at the right level itself;
callers only need the return value if they branch on it.

**Wiring is already done in `main.rs`** — do not re-plumb it:

```rust
let wiring = modules::sleep::connect(&config).await;   // never fails; degrades
let session_locker = wiring.session_locker();          // <- your handle
tasks.spawn(modules::sleep::run(wiring, shutdown_rx.clone()));
tasks.spawn(idle_module_stub(config.idle, session_locker, shutdown_rx.clone()));
```

`idle_module_stub` already takes `_locker: modules::sleep::SessionLocker` as
its second parameter. Stage 5 replaces the stub's body with
`modules::idle::run(idle, locker, shutdown)` and drops the underscore.
`LockTrigger::IdleTimeout` carries `#[allow(dead_code)]` today *only* because
nothing constructs it yet — **remove that attribute when idle.rs starts using
it**, or `-D warnings` will be enforcing a lie.

Two properties Stage 5 depends on:

- **Nested niri**: `connect` falls back to a degraded wiring
  (`UnavailableLogind`) when there is no logind session. In that mode
  `LockedHint` reads fail, and `lock_if_needed` treats an *unreadable* hint
  as **unlocked** and spawns. So idle-lock still works in the nested test;
  the sleep module logs one warning at connect and then sits inert waiting
  for shutdown. That is the expected, correct nested behavior (`CLAUDE.md`'s
  logind caveat), not a bug to chase.
  Caveat worth knowing: a nested niri run from Jordan's *real* session is
  not degraded — `connect` succeeds and reads the **real** session's
  `LockedHint`, which the nested locker does not affect. Expect
  `LockedHint=false` there, so idle-lock will spawn every time.
- The sleep module never consults ScreenSaver inhibits and never will
  (Architecture, binding). Stage 6's flag gates idle only.

## Trait signatures as built

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Logind: Send + Sync + 'static {
    fn acquire_delay_inhibitor(&self) -> BoxFuture<'_, Result<Inhibitor, LogindError>>;
    fn locked_hint(&self) -> BoxFuture<'_, Result<bool, LogindError>>;
}

pub trait LockerSpawner: Send + Sync + 'static {
    fn spawn_locker(&self) -> BoxFuture<'_, Result<(), SpawnError>>;
}
```

Boxed futures, not `async fn` in trait — a trait with `async fn` is not
dyn-compatible and `Arc<dyn Logind>` (fake-swapping) is the whole point.
Same trade as the lockscreen's `auth::Authenticator`/`AuthFuture`.

`Inhibitor` is opaque with **no `release()` method** — `pub struct
Inhibitor { _release: Box<dyn Send + Sync> }`; dropping it is the release,
because that is literally logind's contract (`docs/SIGNALS.md` §3: "released
the moment this file descriptor and all its duplicates are closed"). The
real impl puts the `zvariant::OwnedFd` in there; the test fake puts a
`ReleaseRecorder` whose `Drop` writes to the test journal.

Signal *streams* are deliberately **not** on the trait — `run()` owns them
and feeds the machine `SleepEvent`s, so a fake only answers two questions.

## Confirmation mechanism as actually implemented

Stage 2's decision, unmodified: **`Session.LockedHint == true` is the only
definition of "locked"** in the crate. Read in exactly two places —
`SessionLocker::lock_if_needed` (already-locked skip) and
`SleepMachine::await_lock_confirmation` (the `LockPending` wait).

- **Polling, not `PropertiesChanged`.** `CacheProperties::No` on both
  proxies, so every read is a real `Get` round trip (a stale cache that
  claims "locked" is the one wrong belief that costs severity 1). Poll
  interval `CONFIRMATION_POLL_INTERVAL = 50 ms`.
- **Deadline: `DEFAULT_CONFIRMATION_DEADLINE = 2 s`, a constant in
  `sleep.rs`, not a config knob** (PLAN allowed either; Stage 3's schema is
  canonical and Stage 8 documents it verbatim, and this value is bounded
  above by logind and below by locker startup — not a matter of taste).
  Clamped at runtime by `confirmation_deadline(Option<Duration>)`:
  `min(2 s, 3/5 × InhibitDelayMaxUSec)`, read off the Manager at connect.
  On this machine `InhibitDelayMaxUSec = 5 s` → deadline stays 2 s.
  `checked_mul` because `Duration` arithmetic panics on overflow.
- **Spawn→confirmation duration is logged on every cycle**
  (`confirmation_ms = …`), as Stage 2 asked, so the ~360 ms figure keeps
  being re-measured rather than assumed.
- Deadline expiry ⇒ `warn!` + release + let sleep proceed. Spawn failure ⇒
  `error!` + release **immediately** (no polling: nothing exists that could
  confirm). Already-locked ⇒ release immediately.

## State machine as built

`SleepState { Awake, LockPending, ReadyToSleep }` + `inhibitor:
Option<Inhibitor>` (orthogonal: "which phase" vs "do we hold it").

| Event | Behavior |
|---|---|
| `start()` | acquire inhibitor (`sleep`/`saola-session`/`Lock the session before sleep`/`delay`); skipped entirely when `lock-before-sleep #false` |
| `PrepareForSleep(true)` from `Awake` | `LockPending` → `lock_if_needed(BeforeSleep)` → await confirmation → **always release** → `ReadyToSleep` |
| `PrepareForSleep(true)` from `ReadyToSleep` | `warn!` + no-op (logind restarts / retried suspends; never stacks lockers) |
| `PrepareForSleep(false)` | **re-acquire first**, then set `Awake`, then log (Architecture's ordering, asserted by a test on the shared fake journal) |
| `Lock` | `lock_if_needed(LogindLockSignal)`; independent of sleep state **and** of `lock-before-sleep` |
| `Unlock` | **deliberately inert.** The only thing we could do is kill the locker, which would make `loginctl unlock-session` an auth bypass. Subscribed only so the journal shows it arrived. |

Failure bias, applied consistently: an **unreadable** `LockedHint` counts as
*unlocked* → spawn. Severity 3 (spurious lock) beats severity 1 (exposure).

`lock-before-sleep #false` turns off *only* the sleep concern: no inhibitor,
`PrepareForSleep` inert — but `loginctl lock-session` still works.

**Inhibitor acquisition failure is survivable, not fatal**: 3 attempts,
250 ms apart, then `error!` naming the consequence and carry on (dying would
mean not even spawning a locker on the next `PrepareForSleep`; systemd would
restart us into the same logind that just refused). **Flag for Stage 7's
exposure audit**: there is no background re-try after that — the daemon runs
without a delay inhibitor until the next resume or restart.

## Module task / process-death semantics (a `main.rs` change)

- `run()` returns (→ drops the machine → drops the fd → releases) on the
  shutdown watch. `main.rs` still joins before exiting, which is what makes
  the release ordered rather than raced against teardown.
- If any logind signal stream ends (connection gone, logind restarted),
  `run()` logs at `error!` and returns.
- **`async_main` now `select!`s shutdown against `tasks.join_next()`**: a
  module task returning *before* shutdown was requested logs an error and
  makes the process exit `ExitCode::FAILURE`, so Stage 8's
  `Restart=on-failure` actually fires. This is `CLAUDE.md`'s "task death
  should kill the process, not strand a half-alive daemon", implemented once
  for every module — Stage 5/6 get it for free by just returning.

## Locker spawning

`CommandLocker` (`tokio::process`), detached: `stdin` null, stdout/stderr
inherited (the journal), `Child` dropped immediately — tokio's orphan queue
reaps it on SIGCHLD (hence `enable_all()` on the runtime), so no zombies and
no `wait()`. Never parses output. Spawn = fork/exec only; whether the lock
took effect is `LockedHint`'s answer, never the child's.

`split_command` is plain whitespace splitting — **no shell, no quoting, no
`$VAR`**; a locker path containing spaces is not expressible. Deliberate:
shelling out would make `session.kdl` a code-execution surface. Stage 8's
README should state the limitation.

## Test inventory (23 new, all fake-driven, no D-Bus, `start_paused` clock)

Shared `Journal` (an `Arc<Mutex<Vec<Step>>>` written by *both* fakes) is what
makes cross-trait ordering assertable; `Step::Release` is pushed from the
fake inhibitor's `Drop`.

Startup/shutdown: `startup_takes_the_delay_inhibitor`,
`startup_skips_the_inhibitor_when_lock_before_sleep_is_disabled`,
`inhibitor_acquisition_failure_is_retried_then_survived`,
`shutdown_releases_the_inhibitor`.

Before-sleep: `confirmation_arrives_before_the_deadline`,
`confirmation_times_out_and_sleep_proceeds_anyway`,
`spawn_failure_still_releases_the_inhibitor`,
`already_locked_skips_the_spawn`,
`unreadable_locked_hint_is_treated_as_unlocked`,
`double_prepare_for_sleep_does_not_stack_lockers`,
`prepare_for_sleep_is_inert_when_lock_before_sleep_is_disabled`.

Resume: `resume_reacquires_the_inhibitor_before_anything_else` (exact journal
equality — the `Acquire` is the very next step after `Release`),
`resume_reports_a_failed_reacquire`,
`a_full_sleep_resume_cycle_can_sleep_again` (guards the regression where
resume forgetting to leave `ReadyToSleep` would swallow the *second*
suspend).

Lock/Unlock: `lock_signal_spawns_the_locker`,
`lock_signal_while_locked_does_not_spawn`,
`lock_signal_is_honored_even_when_lock_before_sleep_is_disabled`,
`unlock_signal_takes_no_action`.

Pure helpers: 4 × `confirmation_deadline` (default / 5 s cap / tighter cap
clamps to 600 ms / `Duration::MAX` cannot overflow), 3 × `split_command`.

## Live check — CONFIRMED by Jordan (2026-08-03, real session)

**Result: all steps passed.** Jordan ran the sequence below in his real niri
session (rust 1.97.1) and reported all commands fine. Log evidence
(timestamps his):

- `02:52:25` — session resolved (`…/session/_33`), connected
  (`inhibit_delay_max_ms=5000 confirmation_deadline_ms=2000`), delay
  inhibitor held (`what="sleep" who="saola-session" mode="delay"`).
- `02:53:25` — `loginctl lock-session` → `Lock` signal received → locker
  spawned. (Unlocked via PAM, ran step 4.)
- `02:54:17` — step 4's first lock-session → spawned.
- `02:54:25` — step 4's second lock-session, 8 s later →
  `session already locked (LockedHint=true) — not spawning a second locker`.
  **The already-locked skip is live-confirmed**, as are the two paths no
  unit test could reach: the real `Inhibit() -> fd` call and real
  `LockedHint` reads.

The original sequence follows for reference / re-runs:

```bash
# 1. Start the daemon in the foreground and leave it running.
cd ~/Developer/saola-session && cargo run
```

Expected on stderr within a second or so:

```
INFO saola-session: resolved our logind session   session=/org/freedesktop/login1/session/_33
INFO saola-session: connected to logind           inhibit_delay_max_ms=Some(5000) confirmation_deadline_ms=2000
INFO saola-session: holding a logind delay inhibitor  what=sleep who=saola-session why=Lock the session before sleep mode=delay
```

```bash
# 2. In a SECOND terminal — the inhibitor must be listed.
systemd-inhibit --list | grep -i saola
```

Expected: a row with `WHO=saola-session`, `WHY=Lock the session before
sleep`, `WHAT=sleep`, `MODE=delay`, and the `cargo run` process's UID/PID.
(The retired lockscreen contrib unit's own `who=saola-lockscreen` delay
inhibitor may also still be listed — Stage 8 retires it.)

```bash
# 3. Still in the second terminal — the locker must come up.
loginctl lock-session
```

Expected: `saola-lockscreen` appears on screen, and the first terminal logs

```
INFO saola-session: logind Lock signal received
INFO saola-session: locker spawned   trigger=logind Lock signal
```

Unlock with his password (the locker's own PAM path), then:

```bash
# 4. Already-locked-skips-spawn, live: the second lock-session fires while
#    the session is already locked. Run it, then let the locker come up and
#    leave it up until the command finishes.
loginctl lock-session; sleep 8; loginctl lock-session; sleep 2; pgrep -c saola-lockscreen
```

Expected: exactly **one** locker process (`pgrep -c` prints `1`), and the
first terminal logs the skip rather than a second spawn:

```
INFO saola-session: session already locked (LockedHint=true) — not spawning a second locker  trigger=logind Lock signal
```

Unlock again before continuing.

```bash
# 5. Stop the daemon with Ctrl-C in the first terminal.
```

Expected: `INFO saola-session: released the logind sleep inhibitor
reason=daemon shutting down`, then `saola-session: stopped`, and
`systemd-inhibit --list | grep -i saola-session` no longer shows the row.

**Not part of this check, by rule:** anything that suspends. `systemctl
suspend` end-to-end is Stage 8's documented, Jordan-run test.

## zbus notes / surprises

- **Proxy shapes were verified against live logind, read-only** (a throwaway
  scratchpad probe using the exact proxy definitions from `sleep.rs`: no
  `Inhibit`, no `Lock`, no writes). Results:
  `InhibitDelayMaxUSec = 5000000`; `$XDG_SESSION_ID=3` →
  `/org/freedesktop/login1/session/_33`; `Session.LockedHint = false`;
  all three signal subscriptions register.
- **`GetSessionByPID` fails for a process outside a session scope** —
  observed live: `org.freedesktop.login1.NoSessionForPID`. That is exactly
  the `systemd --user` case Stage 8's unit will be in, so
  `resolve_session_path` has three tiers: `$XDG_SESSION_ID` → `GetSession`,
  then `GetSessionByPID(pid)`, then `GetUserByPID(pid)` → `User.Display`
  `(so)` → path. Tier 3 was confirmed live to return the same `_33`.
  **The session-specific path is mandatory** — `/session/self` and
  `/session/auto` never emit signals (`docs/SIGNALS.md` §3), so a daemon
  subscribed there would silently never see `Lock`.
- **zbus pascal-cases Rust method names**, which would have produced
  `GetSessionByPid`/`GetUserByPid`. Both need explicit
  `#[zbus(name = "GetSessionByPID")]`; `InhibitDelayMaxUSec` likewise needs
  `#[zbus(property, name = "InhibitDelayMaxUSec")]`.
- **The proxy macro generates more than you call** (arg accessors, property
  listeners) and none of it carries `#[allow(dead_code)]`, so `-D warnings`
  rejects the crate. The definitions live in an inline `mod proxies` with
  `#![allow(dead_code)]`; also `gen_blocking = false`.
- **No `futures-util` was added.** `zbus::export::futures_core::Stream` plus
  a four-line `next_signal` (`poll_fn` + `Pin::new`) is all
  `StreamExt::next` ever was, and it is cancel-safe for `select!`.
- **New dev-dependency**: `tokio` with `test-util` (for
  `#[tokio::test(start_paused = true)]`). Dev-only, so the shipped binary
  never gets the pausable clock; documented inline in `Cargo.toml`.
- `Inhibit() -> h` maps to `zbus::zvariant::OwnedFd`, which is `Send + Sync +
  'static` and drops-to-close, so it goes straight into `Inhibitor::new`.
  **This is the one call not exercised outside Jordan's live check** (step 1
  above is precisely its test).

## Open items for Stage 7's audit

1. No background retry after `ensure_inhibitor` exhausts its 3 attempts — the
   daemon can run indefinitely holding nothing, having said so once at
   `error!`.
2. `run()`'s event loop is serial: a `LockPending` sequence blocks other
   signal handling for up to the deadline (~2 s). Documented inline as
   intentional; the queued-signal reasoning is written out there.
3. `connect()` is subscribe-then-inhibit; the reasoning for that ordering
   (and what the other ordering would cost) is in its doc comment.
4. Spawn-failure releases immediately rather than waiting out the deadline —
   deliberate, but it is a place where severity 2 was preferred.
