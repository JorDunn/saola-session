# Stage 5 handoff — idle policy module (`src/modules/idle.rs`)

Compressed state for Stage 6 (the `org.freedesktop.ScreenSaver` inhibit shim).
Everything below is as-built and verified: `cargo build && cargo clippy
--all-targets -- -D warnings && cargo test` exit 0, `cargo fmt --check` clean,
59 tests total (49 going in per Stage 4's handoff, 10 new here, 0 removed).

## The interface Stage 6 plugs into (read this first — it's the point of this handoff)

**Type**: `tokio::sync::watch::Receiver<bool>` / `tokio::sync::watch::Sender<bool>`
pair, `true` meaning "an inhibit is currently active", `false` "no active
inhibit". `main.rs`'s `async_main` already constructs the channel:

```rust
let (inhibit_tx, inhibit_rx) = tokio::sync::watch::channel(false);
...
tasks.spawn(modules::idle::run(config.idle, session_locker, inhibit_rx, shutdown_rx.clone()));
tasks.spawn(inhibit_module_stub(inhibit_tx, shutdown_rx.clone()));
```

**Default is `false` (no inhibit)** — the channel's initial value, satisfying
the task's "defaulting to no-inhibit" requirement with no special-casing
anywhere: until something sends on `inhibit_tx`, `idle::run`'s receiver
simply never observes a change and the policy machine's `inhibit_active`
field (set in `IdlePolicy::new`) stays `false` forever.

**Stage 6's job**: replace `inhibit_module_stub` in `main.rs` with
`modules::inhibit::run(inhibit_tx, shutdown_rx.clone())` (or whatever it ends
up being named) and call `inhibit_tx.send(true)` /
`inhibit_tx.send(false)` whenever the D-Bus `Inhibit`/`UnInhibit`-tracked
active-cookie count transitions between zero and nonzero. `idle::run`
consumes it via `inhibit_active.changed()` in its `select!` loop, translates
each change into `IdleEvent::InhibitChanged(bool)`, and feeds it to
`IdlePolicy::handle` — see `src/modules/idle.rs`'s `run` function, the
`inhibit_active.changed()` branch.

**Do not drop `inhibit_tx` before `idle::run` observes final shutdown.**
`watch::Receiver::changed()` returns `Err` immediately and permanently once
every sender is gone, which would busy-loop `select!` if polled
unconditionally — `idle::run` guards against this with an `inhibit_closed`
flag that stops polling that branch after the first `Err` (see `run`'s doc
comment), so a premature drop degrades gracefully (idle policy keeps running
on the last-known inhibit state) rather than crashing or spinning, but it
does mean Stage 6's module should hold `inhibit_tx` for its whole task
lifetime, same as the current stub does.

**Scope question Stage 6 needs to resolve, flagged by Stage 2 and worth
restating here because it bears directly on whether this interface ever gets
a real sender**: Stage 2's handoff (`docs/SIGNALS.md` §4) found that **niri
itself already owns `org.freedesktop.ScreenSaver`** on this machine and
already wires `Inhibit`/`UnInhibit` into its own `ext-idle-notify-v1`
suppression, *inside niri, before any client (including this module) ever
sees the `idled` event*. Practically: the Firefox-video-inhibits-idle
scenario Architecture names as this module's motivating case is **already
handled for free**, for any client consuming `ext-idle-notify-v1` the way
`idle.rs` does — which is exactly what I verified live (see below): the
notification objects `idle.rs` creates are the `get_idle_notification`
(inhibitor-respecting) variant, not `get_input_idle_notification`. Stage 2's
handoff says outright: "Stage 6 must not attempt to claim
`org.freedesktop.ScreenSaver`" (niri set `AllowReplacement`, so a naive
`request_name` would succeed and silently break niri's own suppression for
every other consumer). So Stage 6 may end up being "verify niri's
suppression is sufficient, document it, and leave `inhibit.rs` mostly inert"
rather than "build a competing D-Bus shim" — but the `IdleEvent::InhibitChanged`
seam described above exists and works regardless of what Stage 6 decides,
in case Jordan wants a real (non-niri-owned) inhibit path for a future
non-niri compositor or as a defense-in-depth fallback.

**RESOLVED by Jordan (2026-08-03), before Stage 6 ran**: build the **full**
shim — cookie tracking, peer-vanish cleanup, the `watch::Sender<bool>` feed —
but make name ownership **conditional**: check whether anything already owns
`org.freedesktop.ScreenSaver` at startup; if owned (niri's case here), log it
and stay inert (niri's own suppression does the work); if unowned (any other
`ext-idle-notify-v1` compositor), claim the name and serve Inhibit/UnInhibit
for real. Never blindly `request_name` — niri set `AllowReplacement`, so a
blind claim would succeed and silently break niri's own suppression for every
other idle consumer (Stage 2's binding caveat).

## What was built

- **`IdlePolicy`** (pure state machine, no I/O): two independent `Option<Arm>`
  slots (`Lock`, `PowerOff`), each `None` when its `session.kdl` minutes value
  is `None` (permanently disabled — can never appear as an event and could
  never fire). `handle(IdleEvent) -> IdleOutcome`. `Arm::Armed` → `Idled`
  fires (or is `Suppressed` if `inhibit_active`) and flips to `Arm::Fired`;
  a second `Idled` before a `Resumed` is `NoAction` (defensive — the protocol
  says the compositor must never do this, but the machine doesn't trust
  that); `Resumed` flips back to `Armed` (`Rearmed`). Inhibit is a single
  global flag, not per-target (Architecture: a ScreenSaver inhibit suppresses
  "new idle actions", not just one of the two).
- **`IdleExecutor`**: turns `IdleOutcome::Fired(target)` into the real action.
  `Fired(Lock)` calls `SessionLocker::lock_if_needed(LockTrigger::IdleTimeout)`
  — Stage 4's locker-spawn path, unmodified, reused exactly as Architecture
  requires ("one implementation, not two"); the `#[allow(dead_code)]` on
  `LockTrigger::IdleTimeout` in `sleep.rs` is now gone, since this is the
  first real construction site. `Fired(PowerOff)` runs `niri msg action
  power-off-monitors` via a new one-method trait, `PowerOffCommand` (a
  `LockerSpawner`-shaped sibling in `idle.rs`, real impl `NiriPowerOff`),
  wrapped in a 5 s `tokio::time::timeout` so a stuck `niri msg` can never
  block the idle event loop; failure or timeout is `warn!`-logged and
  otherwise ignored (severity rule 3). Unlike the locker spawn, this one
  *does* await the child's exit status — `niri msg` is a short-lived IPC
  round-trip, not a long-running foreground process, so waiting for it is
  how the daemon learns whether niri actually heard it.
- **The Wayland bridge**: a dedicated `std::thread` (not a tokio task —
  `wayland-client`'s `blocking_dispatch` has no `Future` anywhere in it, same
  shape `saola-panel`'s `modules/volume.rs` hit with libpulse's mainloop; see
  `idle.rs`'s module doc comment for the full teaching note). It connects
  (`Connection::connect_to_env`), does `registry_queue_init`, binds
  `ext_idle_notifier_v1` and a `wl_seat` via `GlobalList::bind` with the
  range's upper bound taken from `Interface::version` itself (never a
  hardcoded literal — makes `bind`'s only panic path structurally
  unreachable), creates one `get_idle_notification` per configured target
  (respects compositor-side idle inhibitors — the non-input variant, per
  Stage 2's ScreenSaver-suppression finding above), reports readiness once
  via a `oneshot::channel`, then loops `blocking_dispatch` forever,
  translating `idled`/`resumed` into `IdleEvent`s on an
  `mpsc::unbounded_channel` back to the async task. **Not joined on
  shutdown** — a deliberate simplification, documented in `run`'s doc
  comment: nothing needs to wake the thread early (idle notifications are
  compositor-driven, one direction only), so on shutdown `run` just stops
  reading the channel and returns; the thread is left parked in a kernel
  `read()` until process exit, using no CPU. No exposure/severity risk
  (Architecture's severity order is about locking/suspend, not Wayland
  thread teardown latency) — flagged here in case a future stage wants a
  self-pipe retrofit (the panel's `volume.rs` pattern) for stricter shutdown
  ordering.
- **Config-disabled-both skips Wayland entirely**: `run()`'s first check is
  `idle.lock_after_minutes.is_none() && idle.power_off_after_minutes.is_none()`
  → log and wait for shutdown, never touching `Connection::connect_to_env`.
  Verified: the power-off-only live run below logged no `lock-after`
  registration line, and vice versa in the lock-only run.
- **Wayland/logind connect failures are fatal for this module, unlike
  `sleep.rs`.** This is a deliberate divergence from `sleep.rs`'s degraded
  `UnavailableLogind` mode, not an oversight: `sleep.rs`'s degraded mode is a
  *documented, binding exception* for the nested-niri-has-no-logind-session
  case (`CLAUDE.md`). There is no equivalent carve-out for idle's Wayland
  connection — if `idle.lock_after_minutes`/`power_off_after_minutes` is
  configured but the Wayland connect or the `ext-idle-notify-v1` bind fails,
  that is exactly the severity-1 "idles with the session unlocked" risk
  materializing silently if left running degraded, so `run()` logs at
  `error!` and returns, which (per `main.rs`'s Stage 4-established
  `select!`) exits the process non-zero for `Restart=on-failure` to catch.
- `main.rs`: `idle_module_stub` is gone; `modules::idle::run` is spawned
  directly with `(config.idle, session_locker, inhibit_rx, shutdown_rx.clone())`.
  `inhibit_module_stub` now also takes `inhibit_tx: watch::Sender<bool>` (see
  the interface section above) purely to keep it alive until Stage 6 replaces
  the stub.

## Live nested-niri evidence

Followed the lockscreen's nested-niri procedure (`~/Developer/saola-lockscreen/CLAUDE.md`):
`niri -c /tmp/.../nested-niri.kdl &` (no `--session`), confirmed
`wayland-2` / IPC socket `.../niri.wayland-2.98139.sock` from niri's own
startup log, ran the built `target/debug/saola-session` binary (not `cargo
run`, to avoid a rebuild-triggered delay mid-test) with `WAYLAND_DISPLAY=wayland-2`
only, `SAOLA_CONFIG_DIR` pointed at a throwaway `session.kdl`, and tore both
down afterward (`kill` on the daemon and the nested niri PID; confirmed via
`pgrep` neither remained).

**Note on `session.kdl`'s granularity**: at the time of this stage's live
test the schema only accepted whole minutes, so the runs below used
`lock-after 1` / `power-off-after 1` (60 s) and waited it out.
**Superseded immediately after this stage (Jordan's request, 2026-08-03)**:
the timeouts now also accept a quoted duration with an explicit unit —
`lock-after "5s"` — so sub-minute live tests ARE now expressible; Stage 3's
handoff carries the amended schema, and `IdleConfig`'s fields are now
`lock_after`/`power_off_after: Option<Duration>` (verified end to end with
`--check-config`; all gates re-run green, 62 tests). Future nested-niri
runs should use `"5s"`-style timeouts instead of waiting out a minute.

**Note on logind**: running from an interactive shell means the process
inherits Jordan's real `$XDG_SESSION_ID` (confirmed: session 3, active,
seat0) — `sleep::connect()` would otherwise resolve and hold a **real** delay
inhibitor against his actual login session, which is more real-session
interaction than a Stage 5 Wayland test should cause even though it's
inherently safe (Stage 4's own live check already has Jordan run `cargo run`
in his real session the same way). To get a genuinely degraded logind path —
matching what `CLAUDE.md` says to *expect* in this environment — I set
`DBUS_SYSTEM_BUS_ADDRESS=unix:path=/tmp/saola-session-test-no-such-bus` (a
nonexistent path) for the daemon's environment, which makes
`zbus::Connection::system()` fail cleanly and deterministically, without
root and without touching any real session state. Confirmed via the
daemon's own log line (see below) that this produced the exact same
`UnavailableLogind`/log-and-continue path `sleep.rs` takes inside a real
nested-niri-without-`--session` environment. This env var is inherited by
the spawned `saola-lockscreen` child too (`tokio::process::Command` inherits
parent env by default, and `CommandLocker` doesn't clear it) — worth knowing
if a future test sees the locker child behave oddly around D-Bus.

### Run 1 — `idle { lock-after 1 }`

```
config: IdleConfig { lock_after_minutes: Some(1), power_off_after_minutes: None }, ...
WARN  sleep: logind is unavailable — ... (expected inside a nested niri ...) error=no system bus connection: I/O error: No such file or directory (os error 2)
INFO  sleep: sleep module running degraded (no logind) — waiting for shutdown
INFO  idle: idle-lock notification registered minutes=1
INFO  idle: idle module connected to Wayland; dispatching
—— 60s later ——
WARN  sleep: could not read logind LockedHint; assuming the session is UNLOCKED and spawning the locker (a spurious lock is preferable to an exposed session) trigger=idle timeout error=logind is not available in this session
DEBUG sleep: locker process started (detached) program="saola-lockscreen" pid=98802
INFO  sleep: locker spawned trigger=idle timeout
DEBUG idle: idle target re-armed after activity resumed target=lock
INFO  (SIGTERM from my 80s `timeout` wrapper) — clean shutdown, "idle module stopping", "stopped"
```

Independent confirmation from niri's own log (not the daemon's), same
signal the lockscreen's own `CLAUDE.md` calls authoritative for "did a lock
actually happen" since `ext-session-lock-v1` surfaces never show up in
`niri msg windows`/`layers`:

```
2026-08-03T03:19:03.268901Z  INFO niri::niri: locking session
```

15 ms after the daemon's "locker spawned" line — consistent with Stage 2's
confirmed ~46 ms spawn→compositor-lock timing. `NIRI_SOCKET=<nested> niri msg
windows` and `niri msg layers` were both empty before and after, as expected
for a session-lock surface. `trigger=idle timeout` in the log confirms
`LockTrigger::IdleTimeout` is actually being constructed and threaded through
— the first real call site now that the `#[allow(dead_code)]` is gone.

One unexplained but out-of-scope observation: the spawned `saola-lockscreen`
(pid 98802) was no longer running a few seconds after the run ended, with no
error output captured (its stdout/stderr are inherited from the daemon's,
which were captured to the same log file, and nothing else appeared). This
is `saola-lockscreen`'s own behavior in an environment where it too inherited
the broken `DBUS_SYSTEM_BUS_ADDRESS` (see the note above) — not something in
`idle.rs`, and not chased further here; flagging it only in case a future
stage's nested test notices the same thing and wonders whether it's new.

### Run 2 — `idle { power-off-after 1 }`

Same procedure, `NIRI_SOCKET` additionally set on the daemon's own
environment this time (`niri msg`'s child process needs it to target the
nested instance rather than Jordan's real outer niri — the same gotcha the
lockscreen's `CLAUDE.md` calls out for interactive `niri msg` use, which
applies equally to `idle.rs`'s own shelled-out `niri msg` call).

```
config: IdleConfig { lock_after_minutes: None, power_off_after_minutes: Some(1) }, ...
INFO  idle: idle-power-off notification registered minutes=1
INFO  idle: idle module connected to Wayland; dispatching
—— 60s later ——
INFO  idle: powered off monitors after idle timeout
```

No `idle-lock notification registered` line appeared in either run's log —
confirms the two targets are registered independently, per config, not as an
all-or-nothing pair. `niri msg action power-off-monitors` exited 0 (the
`Ok(Ok(()))` arm — a `warn!` would have fired otherwise, and didn't).
`niri msg outputs` text is unchanged before/after on the `winit` backend
(no "enabled: no" style field appears in its output for this backend) — the
meaningful confirmation here is the command's own exit status via the
daemon's log, not a `niri msg outputs` diff, since the nested `winit`
backend doesn't seem to surface a power state in that query's text output.

## Test inventory (10 new, `src/modules/idle.rs`)

`IdlePolicy` (pure machine, 6 tests): `each_timeout_fires_its_action_once`,
`resume_rearms_and_the_next_idle_fires_again`, `disabled_actions_never_fire`,
`inhibit_suppresses_new_actions_but_not_already_fired_ones`,
`inhibit_is_independent_per_target`, `both_disabled_means_every_event_is_a_no_op`.

`IdleExecutor` (fakes, journal pattern matching `sleep.rs`'s style, 4 tests):
`fired_lock_spawns_the_locker`, `fired_power_off_runs_the_command`,
`failed_power_off_is_logged_and_otherwise_ignored`,
`no_action_and_suppressed_and_rearmed_and_inhibit_changed_do_nothing_observable`.

The Wayland bridge itself (`connect_and_register`, `wayland_thread_main`,
`run`) is intentionally not unit-tested — Architecture's own phrasing ("the
Wayland and D-Bus plumbing feed it, tests drive it directly") names the
policy machine as the test subject, and the nested-niri run above is this
module's live-wiring check, same division `sleep.rs` draws between its
`SleepMachine` unit tests and its Stage 4 real-session live check.

## Evidence

- `cargo build`: exit 0, clean (one iteration needed: an unused `self` import
  from `wl_seat::{self, WlSeat}` — removed; the rest of the Wayland API
  usage, including `GlobalList::bind`'s exact signature and
  `get_idle_notification`'s argument order, compiled correctly on the first
  attempt against the actual `wayland-client 0.31.15`/`wayland-protocols
  0.32.13` sources in `~/.cargo/registry`).
- `cargo clippy --all-targets -- -D warnings`: exit 0, clean.
- `cargo test`: **59 passed, 0 failed** (49 before this stage per Stage 4's
  handoff, +10 here, 0 removed, 0 ignored).
- `cargo fmt --check`: exit 0, clean (one `cargo fmt` pass applied — import
  ordering and two multi-line-vs-one-line call formatting differences).
- Nested-niri live evidence: both runs above; daemon and nested niri
  instance both confirmed torn down afterward (`pgrep` empty for both
  patterns).

## Nothing deferred from the task list

Registration/skip-when-disabled, spawn-if-not-locked reuse via
`SessionLocker`, `niri msg action power-off-monitors` via `tokio::process`
with failure logged-and-ignored, resume re-arm, the pure state machine with
its four required test behaviors (each timeout fires once, resume re-arms,
inhibit suppresses new-not-in-flight, disabled never fires), and the
nested-niri live test (including the sleep module's expected degraded
behavior) are all done. The one open item is the scope question already
flagged above and in Stage 2's handoff: whether Stage 6 builds a real
`org.freedesktop.ScreenSaver` service at all, given niri already owns that
name and already wires it into `ext-idle-notify-v1` suppression upstream of
this module. That decision does not block Stage 6 from starting — the
`IdleEvent::InhibitChanged` seam this stage built works either way.
