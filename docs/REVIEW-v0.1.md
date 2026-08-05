# saola-session v0.1 — adversarial exposure and silent-failure review (Stage 7)

**Date:** 2026-08-03 · **Scope:** the whole crate at `src/` (`main.rs`, `config.rs`,
`modules/{mod,sleep,idle,inhibit}.rs`), `Cargo.toml`, `Cargo.lock`, the dependency
graph, and the *packaging assumptions* Stage 8's `contrib/systemd/saola-session.service`
will encode (the unit does not exist yet — auditing the assumption is explicitly this
stage's job).
**Method:** read-only. No source file was modified. Findings are for Stage 8 to fix.

This daemon's failure modes, in the order of severity `PLAN.md`'s Architecture section
and `CLAUDE.md` make binding:

1. **Machine suspends or idles with the session unlocked/exposed.**
2. **Suspend blocked or delayed indefinitely.**
3. **Spurious lock.**

Plus the framing `CLAUDE.md` adds on top, which is the lens most of this review looks
through: **this daemon's characteristic failure is silent absence.** A crashed
lockscreen leaves niri's own lock surface on screen; a crashed — or worse, a *half*-alive
— `saola-session` leaves a machine about to suspend with no inhibitor held, or idling
with a lock that will never fire again, and nothing on screen says so. Everything below
is ranked against that ordering, not against generic CVSS.

---

## Verification status — read this before trusting any "live" claim

- **No suspend/resume round trip has ever been performed against this daemon.** Every
  stage's rules forbid an agent running `systemctl suspend`; Stage 8 documents the
  sequence for Jordan. Every statement in this document about the `PrepareForSleep` →
  `LockPending` → release path is derived from reading the code, `docs/SIGNALS.md`'s
  probes, and the unit tests — **never from an observed suspend.**
- **`Logind::acquire_delay_inhibitor` has been exercised live** (Stage 4's `systemd-inhibit
  --list` check, Stage 6's runs) but **only in the acquire-and-hold direction**. The
  release-then-suspend-proceeds edge has never run against real logind.
- **The `org.freedesktop.ScreenSaver` shim is `Inert` on Jordan's machine and always
  will be while he runs niri** — niri (PID 1336 at Stage 6's check) owns the name, and
  `ZbusNameClaimant::try_claim` deliberately never replaces an existing owner. Stage 6
  exercised the `Active` path only under a private `dbus-run-session` bus. Findings
  **F-1** and **D-1** therefore describe code that is live *by design* for a
  non-niri compositor and dormant on this machine today; that is a statement about
  reachability, not about whether the defect is real.
- **The Firefox-plays-a-video check named in Stage 6's task list was not performed**
  (Stage 6's handoff says so plainly and gives the structural reason). This review did
  not re-attempt it — Stage 7 is read-only and must not run the daemon against the real
  Wayland session.
- **niri's behaviour when an `ext-session-lock-v1` client dies while holding the lock is
  taken from the protocol's guarantee and from `saola-lockscreen`'s `CLAUDE.md`, not
  observed here.** Finding **F-2** flags exactly where that uncertainty changes the
  consequence but not the fix.
- The nested-niri live results in Stage 5's handoff and the `dbus-run-session` results
  in Stage 6's are taken as reported; this review re-ran none of them.

## Evidence run for this review

| Command | Result |
|---|---|
| `cargo build` | exit **0** |
| `cargo clippy --all-targets -- -D warnings` | exit **0**, no diagnostics (run after `cargo clean -p saola-session`, so this is a real compile, not a cache hit) |
| `cargo test` | exit **0** — **76 passed**, 0 failed, 0 ignored |
| `cargo fmt --check` | exit **0** |
| `cargo tree -e normal` | 81 unique crates; see **I-3** |
| `cargo tree -e normal -d` | 3 duplicate pairs, all build-time; see **I-3** |
| `git status --porcelain` before and after | **byte-identical** — no file was modified |

Test inventory behind that 76: `config.rs` 23, `sleep.rs` 25, `inhibit.rs` 14,
`idle.rs` 10, `main.rs` 4.

Green gates are the *start* of this review, not its conclusion. **Every finding below
is invisible to clippy and to all 76 existing tests.** Two of them (F-1, E-3) are
invisible specifically because the existing tests encode the buggy behaviour as the
expected one.

---

## Findings

Severity scale, mapped onto this crate's own ordering: **Critical** (session exposed or
the daemon absent right now) · **High** (severity-1 or severity-2 outcome under a
realistic, reachable condition) · **Medium** (a real failure with a narrower trigger, or
a binding invariant that is factually violated) · **Low** (bounded, or defence-in-depth,
or a documented invariant that is wrong) · **Info** (recorded so a future reader does not
have to re-derive it).

### Summary

| # | Severity | Blocking? | Title |
|---|---|---|---|
| F-1 | High | **must-fix** | An idle-lock suppressed by an inhibit never fires once the inhibit clears — the machine stays unlocked until someone touches the keyboard |
| F-2 | High | **must-fix** | The locker lives in the daemon's cgroup, so systemd's default `KillMode=control-group` kills it on every restart the review's other findings will trigger |
| E-1 | High | **must-fix** | logind unreachable at startup latches a permanently degraded daemon that reports `active (running)` and never locks before sleep again |
| E-2 | High | **must-fix** | A `systemd-logind` restart leaves a stale inhibitor fd that `ensure_inhibitor` refuses to replace, and nothing ends the process to force a fresh one |
| E-3 | Medium | **must-fix** | The idle and sleep paths can both spawn a locker inside the ~360 ms `LockedHint` lag — the "never stack lockers" rule has no cross-task enforcement |
| E-4 | Medium | **must-fix** | A Wayland connect failure at startup is fatal, and systemd's default start-rate limiter turns that into a *permanently* dead unit |
| E-5 | Medium | won't-block | No background retry after `ensure_inhibitor` exhausts its three attempts (Stage 4 flagged this itself) |
| E-6 | Medium | won't-block | The confirmation deadline is a flat 2 s that discards 60 % of this machine's 5 s inhibit budget, against the severity order |
| D-1 | Medium | won't-block | The ScreenSaver shim bounds neither string length nor grant count — a hostile session-bus peer can OOM the lock daemon |
| L-1 | Low | won't-block | `--check-config` installs no tracing subscriber, so every per-knob warning it exists to surface is silently discarded |
| L-2 | Low | won't-block | A failure to install the signal handlers exits **0**, so `Restart=on-failure` will not restart the daemon |
| L-3 | Low | won't-block | The peer-vanish watcher's stream-end comment and journal line both claim the daemon keeps running; it exits immediately |
| L-4 | Low | won't-block | A grant made with an empty `sender` header can never be peer-vanish-cleaned (Stage 6 asked for a second opinion) |
| L-5 | Low | won't-block | With logind degraded, idle-lock stacks a fresh locker every idle cycle |
| L-6 | Low | won't-block | Session teardown races task death: exit 1 and an `ERROR` in the journal at every logout |
| I-1..I-7 | Info | — | Panic surface, the cookie-wrapping question, dependencies, spawn hygiene, unbounded inhibits, `WAYLAND_DISPLAY`, locker environment |

---

### F-1 — an idle-lock suppressed by an inhibit never fires after the inhibit clears

**must-fix**

**Where:** `src/modules/idle.rs:240–247` (`Some(arm @ Arm::Armed)` → `*arm = Arm::Fired`
*before* the `if inhibited` branch), `src/modules/idle.rs:219–222`
(`IdleEvent::InhibitChanged` sets the flag and nothing else).

`IdlePolicy::handle` marks a target `Arm::Fired` on `Idled` **whether or not the action
actually ran**:

```rust
Some(arm @ Arm::Armed) => {
    *arm = Arm::Fired;
    if inhibited { IdleOutcome::Suppressed(target) } else { IdleOutcome::Fired(target) }
}
```

The only thing that ever returns an arm to `Armed` is `IdleEvent::Resumed`
(`:223–229`), and `Resumed` comes from one place: the compositor's
`ext_idle_notification_v1.resumed` event, which fires **only on real seat activity**.
`InhibitChanged(false)` records the flag and returns; it re-evaluates nothing.

So once a target has been suppressed, the *only* way it can ever fire again is a human
touching the keyboard or mouse.

**Failure scenario (concrete):**

1. `session.kdl` has `idle { lock-after 10 }`. Jordan starts a film in Firefox. Firefox
   calls `Inhibit("Firefox", "playing video")`; the shim grants a cookie and sends `true`
   on the watch channel; `IdlePolicy.inhibit_active = true`.
2. He stops touching the keyboard. Ten minutes later `ext-idle-notify-v1` delivers
   `idled` for the `Lock` target. `handle` returns `Suppressed(Lock)` and sets
   `lock = Fired`. Correct so far — that is exactly what the inhibit is for.
3. He falls asleep. The film ends; Firefox calls `UnInhibit`, or Firefox exits and the
   peer-vanish watcher drops the cookie. Either way `Shared::apply(BecameInactive)` sends
   `false`, `handle(InhibitChanged(false))` sets the flag, and **nothing else happens**.
4. The seat is still idle, so no `resumed` is ever emitted. `lock` stays `Fired`
   forever. **The session is never locked, for the rest of the night.**

That is severity 1 — "machine idles with the session unlocked/exposed" — reached through
the exact use case (`Firefox video is the canonical case`) the inhibit module was built
for. Note the sleep path is unaffected: a lid close still locks, per Architecture. It is
the "walk away and leave it" case that is wide open.

The existing test **encodes the bug as the spec**:
`inhibit_suppresses_new_actions_but_not_already_fired_ones`
(`src/modules/idle.rs`, tests) clears the inhibit and then explicitly injects
`p.handle(IdleEvent::Resumed(IdleTarget::Lock))` before asserting the next `Idled` fires.
Remove that injected `Resumed` — i.e. model a user who never comes back — and the
suppressed lock never fires. There is no test for that case at all.

**Reachability today:** on Jordan's machine the shim is `Inert` (niri owns the name), so
`InhibitChanged` is never sent by this daemon and `inhibit_active` is permanently
`false` — F-1 cannot fire there *today*. niri does its own suppression upstream, and
whether niri has the same bug is niri's business, not this crate's. F-1 goes live the
moment the shim goes `Active`: a non-niri compositor, a niri build without its shim, or
any bus where nothing else claims the name. Given the module exists precisely for that
case, "dormant on one machine" is not a reason to ship it.

**Fix sketch for Stage 8:**

Split the arm state so "fired" and "suppressed" are distinguishable, then re-evaluate on
inhibit-clear:

```rust
enum Arm { Armed, Fired, Suppressed }
```

- `Idled` while inhibited → `Arm::Suppressed`, `IdleOutcome::Suppressed(target)`
  (unchanged externally).
- `Idled` while not inhibited → `Arm::Fired`, `IdleOutcome::Fired(target)` (unchanged).
- `Resumed` → `Arm::Armed` for either (unchanged).
- **New:** `InhibitChanged(false)` walks both targets; every one currently `Suppressed`
  becomes `Fired` and yields its action *now*. The seat has already been idle longer than
  the configured timeout, so firing immediately is not a spurious lock — it is the lock
  that was owed and deferred.

`IdlePolicy::handle` currently returns a single `IdleOutcome`; the clean shape is to make
the `InhibitChanged(false)` case return a small `Vec<IdleOutcome>` (or add an
`IdleOutcome::Multiple`), and have `run`'s call site `execute` each in turn. Both call
sites (`src/modules/idle.rs:690` and `:707`) are one line each.

Add tests for exactly the untested path: *suppressed → inhibit clears with no `Resumed`
→ the lock fires*, for each target independently, and *suppressed → `Resumed` first →
inhibit clears → nothing double-fires*.

---

### F-2 — the locker is a child in the daemon's cgroup, so a daemon restart kills the lock screen

**must-fix** (it is one line in a file Stage 8 is about to write, and the rest of this
review makes the triggering restart *likely*, not hypothetical)

**Where:** `src/modules/sleep.rs:742–782` (`CommandLocker::spawn_locker` —
`tokio::process::Command::new(&program).spawn()`), consumed by
`src/modules/sleep.rs:309–346`; and the unit file Stage 8's task list specifies
(`contrib/systemd/saola-session.service`, `Restart=on-failure`).

`spawn_locker` starts the locker as an ordinary child of this process. It does not
`setsid`, does not double-fork, and does not hand the child to `systemd-run --user
--scope`. Under a `systemd --user` unit the child therefore lands in the *unit's* cgroup.

systemd's default `KillMode` for a service is `control-group`: on **stop — including the
stop half of every restart** — systemd `SIGTERM`s, then `SIGKILL`s, **every process left
in the cgroup**, not just the main one. So:

**Failure scenario (concrete):**

1. Idle timeout fires; `saola-lockscreen` is spawned and locks the session.
2. Some hours later niri is restarted, or the Wayland socket hiccups. The Wayland
   dispatch thread's `blocking_dispatch` errors (`src/modules/idle.rs:582–597`), the
   channel closes, `idle::run` breaks (`:710–722`), `main.rs`'s `select!` observes a task
   completing and returns `ExitCode::FAILURE` (`src/main.rs:219–225`) — all of which is
   *correct*, deliberate, and exactly what `CLAUDE.md` asks for.
3. systemd runs the unit's stop job before restarting it, and `KillMode=control-group`
   kills the running `saola-lockscreen` along with the daemon.
4. The lock client is now dead while the compositor still holds the lock. Per
   `ext-session-lock-v1` and per `saola-lockscreen`'s own `CLAUDE.md` ("a crashed
   lockscreen leaves niri's own lock surface on screen (its safety net)"), niri keeps the
   session locked and shows its own fallback surface — which accepts no password. Jordan
   is at a blank screen with no way in short of a VT switch. That is **severity 2**
   (locked out / the machine is unusable), for as long as it takes him to work that out.
5. The restarted daemon does not spawn a replacement locker, because nothing tells it to:
   `SessionLocker::lock_if_needed` is only called from an idle timeout, a `Lock` signal,
   or `PrepareForSleep`, and none of those has just happened.

**The uncertainty, stated plainly:** step 4's *consequence* depends on niri keeping the
session locked after its lock client dies. That is what the protocol requires and what
the sibling repo's `CLAUDE.md` asserts, but it was **not observed by this review**. If a
compositor instead released the lock, the same sequence is **severity 1 — session exposed
by a daemon restart.** The fix is identical either way, and cheap, which is why the
uncertainty does not change the recommendation.

**Fix sketch for Stage 8:**

1. In `contrib/systemd/saola-session.service`, set **`KillMode=process`**, with a comment
   naming this finding: *only the main process is signalled on stop, so a lock screen
   this daemon spawned survives its parent being restarted.* This is the whole fix for
   the common case and costs nothing else — the daemon has no other children worth
   reaping (`niri msg` is short-lived and awaited).
2. Consider also making the daemon's startup self-healing so a restart *re-establishes*
   the lock instead of merely not breaking it: at startup, after `connect`, read
   `LockedHint` once, and if it is `true` while no locker of ours is running, spawn one
   through the existing `lock_if_needed` path. This turns a killed locker into a
   recoverable state rather than a VT-switch. It reuses the single spawn path and is a
   handful of lines; treat it as the belt to `KillMode=process`'s braces.
3. Say both out loud in the README's known-limitations section, because the interaction
   between `Restart=on-failure` and a spawned locker is not obvious to anyone editing
   the unit later.

---

### E-1 — logind unreachable at startup latches a permanently degraded daemon that still reports `active (running)`

**must-fix**

**Where:** `src/modules/sleep.rs:949–986` (`connect`'s `Err` arm builds a
`SleepWiring { signals: None, .. }` around `UnavailableLogind`),
`src/modules/sleep.rs:1149–1153` (`run`'s `let Some(signals) = signals else { … wait for
shutdown … }`).

The degraded mode is required — `CLAUDE.md` makes it binding for nested-niri testing, and
this review agrees the *behaviour* is right there. What is wrong is that it is
**permanent, one-shot, and invisible**:

- `connect_logind()` is called exactly once, from `connect()`, at process start.
- Its failure logs **one** `warn!` and is never retried. Not on a timer, not on a bus
  reconnect, not ever.
- `run` then takes the `signals: None` branch and does `let _ = shutdown.changed().await`
  — it parks until shutdown and does nothing else for the process's entire life.
- `machine.start()` is never reached, so no inhibitor is even attempted.
- The task never returns, so `main.rs`'s task-death-is-fatal handling
  (`src/main.rs:219–225`) never trips. The process stays up. **systemd reports
  `active (running)`.**

**Failure scenario (concrete):**

1. The `systemd --user` unit starts during login, ordered `After=graphical-session.target`.
   The system bus is momentarily unreachable, or `systemd-logind` is mid-`daemon-reexec`,
   or the unit is being restarted by systemd for one of the other reasons in this review
   at a moment when D-Bus is busy. `zbus::Connection::system()` (`:992`) returns `Err`.
2. One `WARN` scrolls past in the journal at boot, phrased — correctly for the nested
   case, misleadingly for this one — as *"expected inside a nested niri … a bug in a real
   session"*.
3. For the rest of the uptime: **no delay inhibitor is held, `PrepareForSleep` is never
   received, `loginctl lock-session` does nothing, and before-sleep locking does not
   exist.** `systemctl --user status saola-session` says `active (running)`.
4. Jordan closes the lid. The machine suspends immediately with the session unlocked and
   resumes showing his desktop.

This is the textbook instance of the failure `CLAUDE.md` calls worse than a crash — "it
*looks* running" — and the code path that produces it is the one place in the crate that
deliberately refuses to die.

**Fix sketch for Stage 8:**

Make degraded mode a *retrying* state rather than a latched one, and make it noisy:

1. Move `connect_logind()` behind a small reconnect loop inside `run`'s degraded branch:
   `select!` between `shutdown.changed()` and a `tokio::time::interval` (30 s is
   plenty — nothing about this needs to be fast) that re-attempts `connect_logind()`.
   On success, install the signals, call `machine.start()`, and fall through into the
   normal loop. This makes the nested case cost one cheap failed connect every 30 s
   (log the retries at `debug!` after the first) and makes the real-session case
   **self-heal**.
2. Log the *ongoing* condition, not just its onset: while degraded, `warn!` once every
   few minutes with a fixed message, so `journalctl --user -u saola-session -p warning`
   shows a standing problem rather than one line at boot. An operator has to be able to
   discover this state after the fact; today they cannot.
3. Give the README an explicit "is it actually working?" check —
   `systemd-inhibit --list | grep saola-session` — and say that `active (running)` alone
   proves nothing. Stage 8's task list already calls for a verification sequence; this
   belongs in it.

---

### E-2 — a `systemd-logind` restart leaves a stale inhibitor fd that is never replaced

**must-fix**

**Where:** `src/modules/sleep.rs:605–609` (`ensure_inhibitor`'s
`if self.inhibitor.is_some() { … return true; }`), `src/modules/sleep.rs:868–878`
(the fd is the inhibitor), `src/modules/sleep.rs:1189–1248` (the signal-stream-ended
paths that are the only thing that would otherwise force a fresh connection).

The inhibitor is an `OwnedFd` handed back by `Inhibit()`; releasing it *is* closing it.
That modelling is elegant and correct. What it cannot express is **"the fd is still open
but the process on the other end is gone."**

When `systemd-logind` is restarted (a systemd package upgrade, `systemctl restart
systemd-logind`, an admin's `daemon-reexec` that does not preserve inhibitor state), the
new logind has no record of our delay lock. Our fd stays open and stays meaningless.

Nothing in this crate notices, and the three things that might all fail to:

- **`ensure_inhibitor` short-circuits.** `self.inhibitor.is_some()` is `true`, so it
  returns `true` immediately without asking logind for anything (`:606–609`). It is
  reporting on a value it holds, not on a lock logind honours.
- **The signal streams do not end, so `run` does not break.** This is the load-bearing
  detail and it is worth being precise about: zbus signal streams are backed by an
  `AddMatch` rule registered with the *bus daemon*, not with logind. The rule's
  `sender='org.freedesktop.login1'` is a well-known name, which the bus daemon resolves
  to the *current* owner at delivery time. A restarted logind re-acquires that name and
  its signals keep matching. So the `None` branches at `:1202`, `:1225` and `:1240` —
  the crate's whole mechanism for "logind is gone, die and get restarted" — **do not
  fire for a logind restart.** They fire for a dead *bus*, which is a different event.
- **No health check exists.** Nothing periodically re-reads anything that would reveal
  the discrepancy.

**Failure scenario (concrete):**

1. `pacman -Syu` upgrades systemd; `systemd-logind` is restarted. saola-session keeps
   running, keeps a stale fd, keeps logging nothing.
2. Jordan closes the lid an hour later. logind emits `PrepareForSleep(true)`, which we
   *do* receive (the match rule survived), so `on_going_to_sleep` runs and spawns the
   locker — good.
3. But logind is **not waiting for us**, because it has no delay inhibitor from this
   daemon. It proceeds to suspend as soon as its other inhibitors clear. The ~360 ms the
   locker needs to get its surfaces up and set `LockedHint` is now an unprotected race
   against the kernel suspending.
4. Lose that race and the machine suspends with the desktop still composited, and resumes
   showing it for however long it takes the locker to catch up — or forever, if the
   locker's own startup did not survive the suspend. **Severity 1.**
5. Ironically, the system then self-heals: `on_going_to_sleep` always releases
   (`:565`), so `inhibitor` is `None` at the next resume and `on_resume` acquires a fresh,
   valid one. The exposure is a **one-shot per logind restart** — which is exactly the
   kind of intermittent, unreproducible failure that never gets diagnosed.

**Fix sketch for Stage 8:**

The right trigger already exists on the bus. In `connect_logind`, additionally build a
`zbus::fdo::DBusProxy` on the **system** connection and subscribe to
`NameOwnerChanged` filtered to `arg0='org.freedesktop.login1'` — the same
`receive_name_owner_changed_with_args` shape `inhibit.rs:535` already uses, so there is a
pattern in-tree to copy. Add it as a fourth stream to `LogindSignals` and a fourth branch
in `run`'s `select!`, mapping to a new `SleepEvent::LogindRestarted`. Its handler is
three lines:

```rust
SleepEvent::LogindRestarted => {
    // The fd we hold refers to a logind that no longer exists.
    self.release_inhibitor("logind restarted — the held fd is stale");
    let held = self.ensure_inhibitor().await;
    EventOutcome::Resumed { inhibitor_held: held }   // or a dedicated variant
}
```

`release_inhibitor` before `ensure_inhibitor` is what defeats the `is_some()`
short-circuit, and doing it in that order is safe: we are about to re-ask immediately,
and holding a stale fd was worth nothing anyway.

Two smaller notes for the same fix:

- The session object path (`resolve_session_path`, `:1063`) is stable across a logind
  restart (`/org/freedesktop/login1/session/_3N`), so the `LogindSessionProxy` does not
  need rebuilding. Worth a comment saying so, since a reader will wonder.
- If Stage 8 would rather not implement the subscription, the fallback is to make this
  loud rather than silent: on `PrepareForSleep(true)`, log at `warn!` when
  `holds_inhibitor()` is true but the acquisition is older than the last observed logind
  start. That is more code than the subscription, so prefer the subscription.

---

### E-3 — idle and sleep can both spawn a locker inside the `LockedHint` lag

**must-fix** (it violates a rule Architecture states in those words)

**Where:** `src/modules/sleep.rs:285–289` (`SessionLocker` is `Clone` and holds only two
`Arc`s — no mutex, no shared "spawn in flight" state), `src/modules/sleep.rs:309–346`
(`lock_if_needed`: read `LockedHint`, then spawn, with nothing between them),
`src/main.rs:196–202` (the same `SessionLocker` is handed to two independent tasks).

Architecture is unambiguous: *"If the session is already locked when
`PrepareForSleep(true)` arrives, skip the spawn — **never stack lockers**."* The
mechanism that implements it is `lock_if_needed`'s `LockedHint` read.

Within a single task that is airtight — `sleep::run`'s loop `await`s each
`handle_event` to completion before the next `select!`, and its doc comment
(`:1165–1176`) reasons this out correctly. But `SessionLocker` is *cloned* into
`idle::run` (`main.rs:198`, via `wiring.session_locker()`), and the two tasks run
concurrently on a multi-thread runtime with **no shared state between them at all**.

The window is not theoretical, and `docs/SIGNALS.md` measures it for us: spawn →
`LockedHint == true` is **≈ 360 ms** warm (§1's observation table, 21:46:41.421). For
that entire window both tasks read `LockedHint == false` and both conclude they must
spawn.

**Failure scenario (concrete):**

1. Jordan walks away. `idle { lock-after 10 }` elapses; `idle::run` gets
   `Idled(Lock)`, calls `lock_if_needed(IdleTimeout)`, reads `LockedHint == false`,
   and spawns `saola-lockscreen`. Elapsed since spawn: 0 ms.
2. ~50 ms later he pulls the lid shut on his way out. logind emits
   `PrepareForSleep(true)`. `sleep::run` calls `lock_if_needed(BeforeSleep)`, reads
   `LockedHint` — **still `false`**, because locker #1 has not reached `Locked(_)` yet —
   and spawns `saola-lockscreen` **#2**.
3. Two lock clients now race for `ext_session_lock_manager_v1.lock`. The loser is sent
   `finished`; per `saola-lockscreen`'s own review (its `docs/REVIEW-v0.1.md`, finding
   L-3) that crate **ignores `finished`** and keeps running and drawing to surfaces the
   compositor is not showing, and if PAM later succeeds it calls `unlock_and_destroy` on
   a finished lock, which is the `invalid_unlock` protocol error and gets the client
   killed by the compositor.

**Direction of the failure:** safe. The session stays locked, held by whichever client
won. This is a severity-3 outcome (a stray, invisible second locker that dies on its
first unlock attempt), not exposure. It is a must-fix anyway because (a) Architecture
states the rule in binding language and the crate does not implement it across its own
two callers, (b) the stray process is exactly the kind of thing that produces an
unreproducible "my lock screen did something weird" report, and (c) the fix is small.

The same race exists between idle-lock and the `loginctl lock-session` path (`Lock`
signal → `sleep::run`), which is *more* reachable than the lid case: pressing a lock
keybind while the idle timeout is elapsing is an ordinary Tuesday.

**Fix sketch for Stage 8:**

A plain mutex around `lock_if_needed` does **not** fix this — it only serialises the two
callers, after which the second still reads a `LockedHint` that has not caught up yet.
The state that is missing is *"a spawn is already in flight"*, and it has to live in the
shared `SessionLocker`:

```rust
#[derive(Clone)]
pub struct SessionLocker {
    logind: Arc<dyn Logind>,
    spawner: Arc<dyn LockerSpawner>,
    /// When we last successfully spawned. Shared across every clone, so the
    /// idle task and the sleep task see each other's spawns.
    last_spawn: Arc<tokio::sync::Mutex<Option<Instant>>>,
}
```

`lock_if_needed` takes the mutex for the whole read-then-spawn sequence, and treats
*"we spawned less than `LOCK_SETTLE_GRACE` ago"* as equivalent to `AlreadyLocked` (a new
`LockAttempt::SpawnPending` variant keeps the log lines honest). `LOCK_SETTLE_GRACE`
should be small — 2 s, i.e. `DEFAULT_CONFIRMATION_DEADLINE`, is the natural value, since
that is already this crate's stated bound on how long a lock may take to confirm.

Two severity checks on that design, both of which it passes:

- **Does the grace ever suppress a lock that was genuinely needed?** Only if a spawn
  succeeded and the locker then failed to lock within 2 s. In the before-sleep case
  `await_lock_confirmation` still runs and still logs the loud `Unconfirmed` warning, so
  the condition is not hidden. In the idle case the target is `Fired` regardless, which
  is the pre-existing behaviour.
- **Does the mutex risk blocking the sleep path?** It is a `tokio::sync::Mutex` held
  across one `LockedHint` round trip plus a `spawn()`, both bounded; and the sleep path
  is the one that would win it, because the idle path releases it as soon as its own
  spawn returns.

Tests: two `SessionLocker` clones, a fake `Logind` whose `LockedHint` stays `false`, and
concurrent `lock_if_needed` calls — assert the fake spawner was called exactly once. That
is the assertion the crate is currently missing.

---

### E-4 — a Wayland connect failure at startup is fatal, and systemd's start-rate limiter makes that permanent

**must-fix** (mostly in the unit file, i.e. Stage 8's own deliverable)

**Where:** `src/modules/idle.rs:649–655` (thread spawn failure → `return`),
`src/modules/idle.rs:657–667` (`Connection::connect_to_env()` /
`registry_queue_init` / `bind` failure → `return`), consumed by
`src/main.rs:219–225` (any task returning → `ExitCode::FAILURE`).

Returning from `idle::run` is fatal to the process by design, and for a *mid-run*
Wayland death that is exactly right — the compositor is gone, the whole session is being
rebuilt, dying loudly is correct. For a **startup** failure it composes badly with the
restart policy Stage 8 is about to write.

systemd's defaults are `RestartSec=100ms`, `StartLimitBurst=5`,
`StartLimitIntervalSec=10s`. A unit that fails immediately on start therefore burns its
entire budget in well under a second, after which systemd stops trying and leaves the
unit `failed` — **permanently, until someone runs `systemctl --user reset-failed`.**

**Failure scenario (concrete):**

1. The unit is ordered `After=graphical-session.target` (Stage 8's task list says so).
   `graphical-session.target` being reached does not guarantee niri's Wayland socket is
   accepting connections, nor that `WAYLAND_DISPLAY` has been imported into the user
   manager's environment (that requires niri to have run `systemctl --user
   import-environment` / `dbus-update-activation-environment`, which is a separate,
   later, racy step).
2. `Connection::connect_to_env()` fails. `idle::run` logs and returns. Process exits 1.
3. systemd restarts it 100 ms later. Same race, same failure. Five times in half a
   second.
4. The unit is now `failed` and stays that way. **There is no idle lock and no
   before-sleep lock for the entire session**, and — as in E-1 — nothing on screen says
   so. Severity 1, at every boot where the race is lost.

Note the perverse severity inversion: `idle.rs`'s doc comment justifies the fatal
treatment on the grounds that this module "has no lower-severity fallback to degrade to"
(`src/modules/inhibit.rs:636–640` states the comparison). But taking the *process* down
also takes down `sleep.rs`, whose concern is strictly higher-severity. Dying to protect
a severity-1-adjacent idle lock costs the actual severity-1 before-sleep lock.

**Fix sketch for Stage 8 (do all three):**

1. **In the unit:** `RestartSec=2s` and `StartLimitIntervalSec=0` (which disables the
   rate limiter entirely). A daemon whose absence is a severity-1 condition should retry
   forever rather than give up after half a second. If a limiter is wanted at all, make
   it generous: `StartLimitBurst=10` over `StartLimitIntervalSec=300`.
2. **In the code:** make the *startup* Wayland connect retry rather than exit. The
   cleanest small version keeps the fatal treatment for a *dispatch* failure (which
   genuinely means the compositor died) and adds a bounded-backoff retry loop around
   `connect_and_register` inside `wayland_thread_main` — the thread already owns this
   decision and `ready_tx` already carries the outcome, so this is contained. A daemon
   that starts before its compositor should wait for it, not die at it.
3. **In the unit, additionally:** `After=niri.service` (or whatever names the
   compositor) plus `PartOf=graphical-session.target`, so the ordering is against the
   thing that actually provides the socket rather than against a target that merely
   correlates with it.

---

### E-5 — no background retry once `ensure_inhibitor` exhausts its three attempts

won't-block (Stage 4 flagged this itself and asked Stage 7 to rule on it; this is the
ruling)

**Where:** `src/modules/sleep.rs:605–644`.

Three attempts, 250 ms apart, then an `error!` naming the consequence and carry on. The
`error!` copy is genuinely good — it says "the next suspend will not be delayed for the
locker, so the session may reach sleep before the lock surface is up" — so this is not a
silent failure in the strict sense. But it is a **one-shot** one: nothing retries until
the next `PrepareForSleep(false)`, which on a machine that never suspends is never.

**Assessment.** Stage 4's reasoning for not dying (dying would mean not even spawning a
locker on the next `PrepareForSleep`, and systemd would restart us into the same logind
that just refused) is **correct and should be kept**. What is missing is only the
retry. A 750 ms budget is very short for "logind is momentarily busy".

**Failure scenario:** the daemon starts (or is restarted, per E-4) during a burst of
system-bus activity at login. Three `Inhibit()` calls fail inside 750 ms. The daemon runs
all day with no delay inhibitor, having said so once, seven hours ago, in a journal
nobody is reading.

**Fix sketch for Stage 8:** fold this into E-1's fix — the same periodic tick that
retries `connect_logind` should also call `ensure_inhibitor` when `holds_inhibitor()` is
false and `lock_before_sleep` is true, and re-log at `warn!` while the condition persists.
One loop, three findings' worth of self-healing (E-1, E-2's re-acquire, E-5).

---

### E-6 — the confirmation deadline throws away 60 % of the available inhibit budget

won't-block, but reconsider the constant

**Where:** `src/modules/sleep.rs:113` (`DEFAULT_CONFIRMATION_DEADLINE = 2 s`),
`src/modules/sleep.rs:706–715` (`confirmation_deadline`).

```rust
Some(max) => max.checked_mul(3).map(|b| b / 5)
    .unwrap_or(DEFAULT_CONFIRMATION_DEADLINE)
    .min(DEFAULT_CONFIRMATION_DEADLINE),
```

On Jordan's machine `InhibitDelayMaxUSec` is 5 s (`docs/SIGNALS.md` §3), so the
three-fifths rule computes 3 s — and is then clamped by the flat 2 s, which wins. **The
three-fifths logic never binds on this machine**; the effective deadline is always the
constant, and it uses 2 s of a 5 s budget.

The severity order says the *first* thing to protect is "machine suspends with the
session exposed", and the second is "suspend blocked or delayed indefinitely" — where
"indefinitely" is systemically impossible because logind's cap forces the issue at 5 s
regardless of what this daemon does. Given that, giving back 3 s of headroom buys nothing
against severity 2 (logind is going to wait that long anyway if any other inhibitor is
holding) and costs real margin against severity 1 on a cold boot, a loaded machine, or a
locker whose own startup is slower than the 360 ms Stage 2 measured *warm* — which is
plausible, since `saola-lockscreen`'s own review (finding H-1) documents a synchronous
full-size wallpaper decode on its pre-lock path.

**Failure scenario:** first suspend after a cold boot. The locker's wallpaper is not in
page cache; its decode plus surface creation exceeds 2 s. `await_lock_confirmation`
returns `None`, we log the `Unconfirmed` warning and release. logind — which would have
waited three more seconds — suspends. The lock surface comes up during or after the
suspend; the desktop is composited and visible at resume for however long that takes.

**Fix sketch for Stage 8:** raise the constant to 4 s and change the ratio to four
fifths, i.e. `max * 4 / 5` clamped to 4 s. On this machine that yields 4 s, leaving 1 s
of margin for the release itself, which is ample (releasing is a `close()`). Keep the
`checked_mul` — it is correct and is there for the no-panic rule. Update the constant's
doc comment, which currently reasons its way to 2 s from "the machine may have several
inhibitors and logind's cap applies to the sum" — that reasoning is backwards: if
*others* are also delaying, logind is waiting for them anyway and our extra seconds are
free; if we are the only one, the whole 5 s is ours.

This is a knob, not a defect, which is why it does not block. But the current value was
chosen for the wrong reason.

---

### D-1 — the ScreenSaver shim bounds neither string length nor grant count

won't-block on Jordan's machine (the shim is `Inert` there); must-fix before anyone runs
this under a compositor that does not own the name

**Where:** `src/modules/inhibit.rs:450–481` (`inhibit`, which takes
`application_name: &str` and `reason_for_inhibit: &str` straight from the wire),
`src/modules/inhibit.rs:174–192` (`InhibitStore::inhibit`, an unbounded `HashMap::insert`).

`org.freedesktop.ScreenSaver` is an **unauthenticated session-bus service**: any process
on the user's session bus may call it, with no policy check of any kind. This module
applies no bound to:

- **String length.** `Grant { app, reason }` stores both caller-controlled strings
  verbatim. The only ceiling is D-Bus's own 128 MiB per-message limit.
- **Grant count.** `next_cookie` increments and `grants.insert` never refuses. Memory is
  `O(total live grants)` with no cap, per-peer or global.
- **Log volume.** Every successful `Inhibit` emits an `info!` containing both strings in
  full (`:472–479`). A peer calling `Inhibit` with 100 MiB strings writes 100 MiB to the
  journal per call, per grant, plus again on every peer-vanish line (`:575–586`, which
  `join(", ")`s every dropped grant's app and reason into one message).

**Failure scenario (concrete):** a buggy or hostile process on the session bus loops
`Inhibit("A".repeat(1_000_000), "B".repeat(1_000_000))`. Each iteration costs ~2 MB of
resident memory that is never freed while the peer lives, plus ~2 MB of journal. A few
thousand iterations — seconds of work — and the daemon is killed by the OOM killer or by
`user@.service`'s `MemoryMax` if one is set. **The lock daemon is now gone**, which is
severity 1 by the "silent absence" rule, achieved without a single privileged operation.
The amplification factor is the finding: one small D-Bus call from an unprivileged peer
costs this daemon unbounded memory *and* unbounded journal.

Realistic threat model, stated fairly: this is one user's own session bus running their
own apps, so "hostile peer" mostly means "buggy peer". Stage 6 judged that acceptable for
its scope and said so. This review agrees on the *priority* and disagrees on the
*conclusion* — the mitigation is ten lines and removes an unbounded resource from a
daemon whose absence is the worst outcome in the crate.

**Fix sketch for Stage 8:**

```rust
const MAX_FIELD_LEN: usize = 256;      // app / reason, truncated not rejected
const MAX_GRANTS_PER_PEER: usize = 64;
const MAX_GRANTS_TOTAL: usize = 1024;
```

- Truncate `application_name` / `reason_for_inhibit` to `MAX_FIELD_LEN` **on a char
  boundary** (`s.char_indices().nth(MAX_FIELD_LEN)` — not byte slicing, which panics
  mid-codepoint and would violate the no-panic rule) before storing *and* before logging.
- In `InhibitStore::inhibit`, refuse past either cap: return a cookie (the freedesktop
  interface has no error return, and a peer that gets a D-Bus error where it expected a
  `u32` will handle it worse than it handles a cookie) but do **not** insert the grant,
  and `warn!` once per peer per cap breach. Not storing the grant means the inhibit is
  ignored, i.e. idle actions keep firing — the *safe* direction under the severity order.
- Rate-limit the per-grant `info!`: log the first N per peer, then a periodic summary.

Unit tests are trivial here because `InhibitStore` is already pure: assert the caps hold,
assert truncation is char-boundary-safe on a multi-byte input, assert a capped peer
cannot suppress idle.

---

### L-1 — `--check-config` discards every warning it exists to surface

won't-block; three lines

**Where:** `src/main.rs:83–91` (the `Cli::CheckConfig` arm) versus `src/main.rs:108–112`
(`run`, which calls `init_tracing()` *before* `SessionConfig::load()`).

`init_tracing()` is called only from `run()`. The `--check-config` arm calls
`SessionConfig::load()` with no subscriber installed, so every `tracing::warn!` the
loader emits goes nowhere — `tracing` drops events silently when no subscriber is set.

The loader's warnings are the entire diagnostic surface for a bad config
(`src/config.rs:213–217` the whole-document parse failure, `:247–249` a bad `locker`,
`:259–262` a bad `lock-before-sleep`, `:362–366` a bad timeout).

**Failure scenario:** Jordan writes `idle { lock-after "10" }` — a suffix-less string,
which `parse_suffixed_duration` deliberately rejects (`src/config.rs:375–387`). He runs
`saola-session --check-config` to check his work. It prints `lock_after: None` and exits
0, with **no indication that a line in his file was ignored or why**. He concludes idle
policy is off because he did something else wrong. The one warning that would have told
him — *"idle.lock-after \"10\" is not a positive whole number of minutes or a duration
string like \"30s\"/\"5m\" — ignored"* — was written and thrown away.

Stage 8's README is going to point operators at this flag, which is what turns a cosmetic
gap into a documentation lie.

**Fix sketch:** call `init_tracing()` at the top of the `CheckConfig` arm too. Since
`--check-config` is a human-facing command, prefer a filter defaulting to `warn` there
(`EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))`) so the
output is the parsed config plus exactly the complaints, and still honours `RUST_LOG`.
Easiest shape: give `init_tracing` a `default_level: &str` parameter; two call sites.

---

### L-2 — a signal-handler installation failure exits **0**, so `Restart=on-failure` will not restart

won't-block (the trigger is exotic); the fix is one line

**Where:** `src/main.rs:257–279` (`wait_for_shutdown_signal` returns early, having logged,
if `signal(SignalKind::terminate())` or `signal(SignalKind::interrupt())` fails),
`src/main.rs:214–218` (that branch of the `select!` resolves to `ExitCode::SUCCESS`).

`wait_for_shutdown_signal`'s error path is defensible on its own terms — its doc comment
says spinning forever pretending shutdown can still happen cleanly would be worse, and
that is true. But the caller cannot distinguish "returned because a signal arrived" from
"returned because it could not install a handler", and treats both as an
operator-requested stop worth `ExitCode::SUCCESS`.

**Failure scenario:** file-descriptor exhaustion in the user manager, or any other
transient reason `signal()` fails, at the moment the daemon starts. The daemon logs an
error, immediately tears down all three module tasks, releases the inhibitor, and exits
**0**. systemd's `Restart=on-failure` sees a clean exit and does nothing. The unit shows
`inactive (dead)`, which at least does not *lie* the way E-1's `active (running)` does —
but the machine has no lock daemon and nothing will bring it back.

**Fix sketch:** have `wait_for_shutdown_signal` return a `bool` (or a two-variant enum)
saying whether it stopped because of a real signal, and make the `select!` arm choose
`ExitCode::FAILURE` when it did not. Alternatively, and even simpler, install both
handlers *before* spawning the module tasks and return `ExitCode::FAILURE` from
`async_main` directly if either fails — a daemon that cannot hear SIGTERM has failed to
start, and saying so at startup is cleaner than discovering it during shutdown.

---

### L-3 — the peer-vanish watcher's stream-end comment and journal line are both wrong

won't-block; it is a documentation defect, but of the kind this crate's own audit trail
depends on

**Where:** `src/modules/inhibit.rs:595–615` (the `None` branch's comment and its
`error!` copy), against `src/modules/inhibit.rs:713–715` (`watch_for_vanished_peers` is
`.await`ed *inline* in `run`, so returning from it returns from `run`) and
`src/main.rs:219–225` (a returning task is fatal).

The comment says the stream ending is *"scoped to this task only (not the whole
process)"*, and the `error!` an operator reads in the journal says
*"Inhibit/UnInhibit continue to work, but a crashed inhibiting app's cookie will now leak
until the daemon restarts"*.

Neither is true. `watch_for_vanished_peers` is not a separate `tokio::spawn`; it is
awaited as the tail of `run` (`:715`). Returning from it returns from `run`, `run` *is*
the `JoinSet` task, and `main.rs`'s `select!` treats any task returning before shutdown
as fatal. The process exits `ExitCode::FAILURE` within milliseconds. `Inhibit`/`UnInhibit`
do **not** continue to work; nothing continues to work.

The behaviour (die and get restarted) is arguably the right one. The problem is that the
crate's own comments now assert something a future maintainer will reason from, and the
journal tells an operator to expect a degraded-but-running daemon when what they will
actually find is a restart. That is the same class of defect as the sibling review's L-1.

Related, and worth Stage 8 resolving in the same edit: `run`'s doc comment
(`src/modules/inhibit.rs:633–640`) says this module is *"Never fatal to the process"* and
lists three failure paths that indeed are not. This is a fourth that is.

**Fix sketch:** pick one and make the comments match it.
- *Keep the fatality* (recommended — a dead `NameOwnerChanged` stream on a connection we
  still hold is a genuine anomaly): fix the comment and rewrite the `error!` to say the
  daemon is stopping so it will be restarted, and amend `run`'s doc comment to name this
  as the one fatal path.
- *Or make it truly scoped*: `tokio::spawn` the watcher and have `run` await shutdown
  independently — but then the process really does keep serving `Inhibit`/`UnInhibit`
  with no peer-vanish cleanup, which is the leaked-cookie condition Architecture calls
  severity 1. Prefer the first.

---

### L-4 — a grant made with an empty `sender` header can never be peer-vanish-cleaned

won't-block; Stage 6 explicitly asked for a second opinion on this, so here it is

**Where:** `src/modules/inhibit.rs:464` —
`let peer = header.sender().map(ToString::to_string).unwrap_or_default();`

Stage 6's handoff describes the shape correctly: a grant stored with `peer == ""` can
never match a `NameOwnerChanged` for a real unique name, so `peer_vanished` can never
drop it. It could only ever be released by an explicit `UnInhibit` with the right cookie.
A peer that made such a grant and then crashed would leave a **permanent** inhibit, i.e.
idle-lock never fires again for the process's lifetime — the severity-1 leaked-cookie
shape Architecture names.

**The ruling: "believed unreachable" is correct, and it is also not the right thing to
rely on.** The precondition genuinely does not occur — a message routed by `dbus-daemon`
always carries a `sender` header; the only way to receive one without is a peer-to-peer
connection, and this module reaches the bus exclusively through
`zbus::Connection::session()` (`:642`). But `unwrap_or_default()` converts an impossible
condition into a *silently degraded permanent* one, and the whole point of this crate's
severity ordering is not to do that when the alternative costs three lines.

**Fix sketch:**

```rust
let Some(peer) = header.sender().map(ToString::to_string) else {
    warn!("ScreenSaver Inhibit arrived with no sender header — refusing to track an \
           untrackable grant (it could never be cleaned up if the caller crashed)");
    return 0;  // a cookie the caller may UnInhibit harmlessly; nothing is stored
};
```

Not storing the grant means the inhibit is ignored, so idle actions keep firing — the
safe direction, and the same bias `SessionLocker::lock_if_needed` already applies to an
unreadable `LockedHint`. Add a `warn!` so the impossible case, if it ever happens, is
visible rather than inferred.

---

### L-5 — with logind degraded, idle-lock stacks a fresh locker every idle cycle

won't-block (nested-niri only), but it will be seen during Stage 8's live testing and
should not be mistaken for a new bug

**Where:** `src/modules/sleep.rs:927–935` (`UnavailableLogind::locked_hint` always
`Err`), `src/modules/sleep.rs:310–322` (an unreadable `LockedHint` is treated as
*unlocked*).

The failure bias is right and this review endorses it: an unreadable `LockedHint` must
mean "spawn", because severity 3 beats severity 1. But in degraded mode `LockedHint` is
*permanently* unreadable, so the already-locked check is permanently defeated, and
`lock_if_needed` spawns unconditionally on every call.

**Failure scenario (this is Stage 5's nested test, and Stage 8 will re-run it):** nested
niri, `idle { lock-after "10s" }`. Idle timeout fires → locker #1 spawns. Jordan types a
wrong password (activity → `resumed` → the target re-arms). He stops typing. Ten seconds
later → `Idled` → `lock_if_needed` → `LockedHint` unreadable → **locker #2 spawns on top
of locker #1**. Repeat indefinitely. Each cycle also emits the `warn!` about assuming the
session is unlocked, so the journal does say what is happening — but it reads like a bug
report rather than expected behaviour.

**Fix sketch:** E-3's `last_spawn` grace fixes this too, for free — a spawn 10 s ago has
aged out of a 2 s grace, so it does not, actually. If Stage 8 wants this closed as well,
the cheap version is a `Option<Child>`/liveness check in `CommandLocker` (do not spawn if
the previously spawned child is still running), or simply documenting it in the README's
nested-testing section as expected in a logind-less environment. **Documenting it is
sufficient** — it is severity 3 in an environment that by definition is not a real
session.

---

### L-6 — session teardown races task death: exit 1 and an `ERROR` at every logout

won't-block; cosmetic, but it will pollute the journal Stage 8 tells Jordan to read

**Where:** `src/main.rs:214–226` — the `select!` between `wait_for_shutdown_signal()` and
`tasks.join_next()` has **no `biased;`**, unlike every module's own loop
(`sleep.rs:1182`, `idle.rs:679`, `inhibit.rs:554`, all of which do).

At logout, two things happen at once: systemd sends SIGTERM to the unit (via
`PartOf=graphical-session.target`), and niri exits, killing the Wayland socket. The
second makes `idle::run`'s `blocking_dispatch` fail, the channel close, and the task
return. Whichever the `select!` happens to poll first decides the exit code.

Lose the coin flip and every logout logs *"a module task stopped on its own — exiting
non-zero so the daemon is restarted rather than left running without one of its
concerns"* at `error!` and exits 1. Harmless (systemd is running a stop job, so it will
not restart), but it means `journalctl --user -u saola-session -p err` has a false
positive in it after every session — which is precisely the query Stage 8's docs should
be telling Jordan to trust.

**Fix sketch:** add `biased;` as the first line of `async_main`'s `select!`, matching all
three modules. Shutdown then always wins a tie, and a genuine mid-session task death is
still caught on the next poll (it is a completed `JoinSet` entry; it does not go away).

---

### Info

**I-1 — the panic surface, precisely: zero hits.** A per-file scan for `unwrap()` /
`.expect(` / `panic!` / `unreachable!` / `todo!` / `unimplemented!` / `assert`, restricted
to lines **above** each file's `#[cfg(test)]` boundary (`main.rs:295`, `config.rs:389`,
`sleep.rs:1263`, `idle.rs:729`, `inhibit.rs:739`), returns **zero code hits across all six
source files** — the only matches are the strings inside doc comments discussing the rule.
A second scan for indexing (`[…]`), `as` casts to integer types, `unsafe`, and infix
arithmetic on runtime paths returns exactly **one** arithmetic site,
`src/config.rs:351`'s `Duration::from_secs(u64::from(n) * 60)`, which cannot overflow:
`n` is already narrowed to `u32` by `u32::try_from(n).ok().filter(|n| *n > 0)` on the
line above, and `u32::MAX * 60 ≈ 2.6 × 10¹¹` is four orders of magnitude inside `u64`.
There is **no `unsafe` anywhere in the crate**. The non-panicking `unwrap_*` family is
used correctly throughout (`unwrap_or`, `unwrap_or_else`, `unwrap_or_default`,
`ok().filter()`), and the three places where a panic was *architecturally* possible are
each closed deliberately and commented as such: `confirmation_deadline`'s `checked_mul`
(`sleep.rs:710`), `parse_suffixed_duration`'s `checked_mul` (`config.rs:386`),
`timeout_millis`'s `u32::try_from(...).unwrap_or(u32::MAX)` (`idle.rs:98–100`), and
`connect_and_register`'s `1..=ExtIdleNotifierV1::interface().version` range, which makes
`GlobalList::bind`'s documented "compile-time programmer error" panic structurally
unreachable rather than merely unlikely (`idle.rs:499–500`). `Cargo.toml` declares **no
`[profile]` section**, so both profiles use `panic = "unwind"` — which matters, because it
is what lets `JoinSet` catch a task panic and lets `main.rs:236–244` log it, and what lets
the Wayland thread's unwind drop `event_tx` and close the channel that `idle::run` treats
as fatal. `InhibitStore`'s guarded critical sections were re-read specifically to answer
Stage 6's third open question: `inhibit` (a `wrapping_add`, a `HashMap::insert`, three
`Into<String>` conversions), `uninhibit` (`HashMap::remove`), `peer_vanished` (iterate,
collect, remove) and `active_count` (`len`) contain **no panic path**, so
`Shared::lock_store`'s `unwrap_or_else(|poisoned| poisoned.into_inner())` can only ever
absorb a poisoning that cannot occur. The reasoning holds; keep the recovery.

**I-2 — the cookie `wrapping_add` question, ruled on.** Stage 6 flagged
`InhibitStore::inhibit`'s `next_cookie.wrapping_add(1)` (`inhibit.rs:182`) and asked
Stage 7 to disagree if it wanted to. It does not — but the reason recorded in the module
doc comment is weaker than the real one, so here is the real one. Two paths reach a wrap:
(a) 2³² *concurrent* grants, which D-1 shows is an out-of-memory event long before it is
a wrap; (b) 2³² sequential `Inhibit`/`UnInhibit` pairs, which is bounded by D-Bus round
trips — call it tens of thousands per second at best, i.e. **days** of sustained traffic.
And the outcome even then is *safe by direction*: the wrapped cookie collides with a
long-lived grant, `HashMap::insert` overwrites it, and the honest app's inhibit is
**lost** — meaning idle actions resume, which is severity 3 (a spurious lock), never
severity 1. A wrapping counter whose worst case is "an inhibit stops working" is the
correct choice for this crate. **Out of scope confirmed; no change needed.** Fix D-1's
grant cap and path (a) closes too.

**I-3 — dependency review: unusually clean.** `cargo tree -e normal` resolves to **81
unique crates** — very small for a daemon that speaks both D-Bus and Wayland, and a direct
consequence of Stage 1's `default-features = false` discipline. The direct set is exactly
what `CLAUDE.md` documents: `tokio 1.53.1` (`rt-multi-thread`, `macros`, `signal`, `time`,
`sync`, `process`), `zbus 5.18.0` (`default-features = false, features = ["tokio"]`, the
same shape `saola-panel` uses), `wayland-client 0.31.15`, `wayland-protocols 0.32.13`
(`staging` + `client`), `kdl 6.7.1`, `tracing 0.1.44`, `tracing-subscriber 0.3.23`
(`env-filter`). **No C-library bindings beyond `libc`** — no `openssl`, no `native-tls`,
no `dbus-sys`; `wayland-sys 0.31.11` is present as a transitive of `wayland-backend` but
the Rust backend is the one in use, so no `libwayland-client.so` link is required.
`cargo tree -e normal -d` reports **three** duplicate pairs, every one of them build-time
only and none of them in the runtime graph: `syn 2.0.119` / `syn 3.0.3` (zbus's derive
stack versus `serde_derive`/`tokio-macros`/`async-trait`), and `winnow 0.7.15` /
`winnow 1.0.4` (`kdl` versus `zvariant`/`zbus_names`/`toml_edit`). That is ordinary
ecosystem skew, it costs compile time and nothing else, and it is not this crate's to
fix. Nothing in the tree is unmaintained: `lazy_static 1.5.0` (via
`tracing-subscriber` → `sharded-slab`) is in maintenance mode rather than abandoned, and
is the only crate in the graph anyone would raise an eyebrow at. Versions are current
across the board. **`cargo audit` and `cargo deny` are not installed on this machine**, so
no advisory-database check was performed for this review — Stage 8 should consider wiring
one of them into CI, the same recommendation the sibling review made.

**I-4 — spawn hygiene: clean.** Verified against all three of Architecture's requirements.
*Cannot block the event loop:* `CommandLocker::spawn_locker` (`sleep.rs:743–781`) calls
`Command::spawn()`, which returns as soon as fork/exec succeeds and never `.await`s a
child; `NiriPowerOff::power_off_monitors` (`idle.rs:296–317`) does await
`status()`, but the call site wraps it in `tokio::time::timeout(POWER_OFF_TIMEOUT = 5 s)`
(`idle.rs:363–378`), so a wedged `niri msg` cannot stall the idle loop indefinitely — and
the asymmetry between the two (await the short-lived IPC helper, never await the
long-lived locker) is correct and is explained at both sites. *No output parsing:* the
locker's stdout/stderr stay inherited and are never read (`sleep.rs:753–756`);
`niri msg`'s are sent to `/dev/null` and only its exit status is inspected
(`idle.rs:304–308`). Neither ever reads a byte from a child. *No zombies:* the locker's
`Child` handle is explicitly `drop`ped (`sleep.rs:773`), which hands it to tokio's orphan
queue, reaped on `SIGCHLD` — which works only because `main.rs:120–123` builds the runtime
with `enable_all()`, and the comment at the drop site says exactly that. *No shell:*
`split_command` (`sleep.rs:725–729`) is plain whitespace splitting into `argv` with no
shell anywhere, so `session.kdl` is not a code-execution surface for anything that can
write it; the cost (a locker path containing spaces is inexpressible) is documented at
the site and is the right trade. *Empty command:* `split_command` returns `None` and
`spawn_locker` maps it to a `SpawnError`, which `lock_if_needed` logs at `error!` — and
`config.rs`'s loader guarantees a non-empty `locker` anyway, so this is belt and braces.
The one gap in spawn hygiene is the *cross-task* double-spawn, which is E-3, and the
child's cgroup membership, which is F-2.

**I-5 — an inhibit has no upper bound and no periodic reminder.** Once granted, a cookie
suppresses idle actions for as long as its peer lives, with no timeout and no ceiling.
That is what the freedesktop interface specifies, so it is not a defect — but it means a
buggy app that inhibits and never releases (and never crashes, so peer-vanish never
helps) silently disables idle-lock forever, with exactly one `info!` at grant time to
show for it. The before-sleep lock is unaffected, per Architecture, so this is bounded to
the idle concern. If Stage 8 wants cheap insurance, a periodic `info!` while
`is_active()` (every 15 minutes, naming the holding apps) turns an invisible state into
a greppable one. Not required.

**I-6 — `WAYLAND_DISPLAY` under `systemd --user` is load-bearing for more than the idle
module.** The locker inherits the daemon's environment (`spawn_locker` sets no `env`).
Under a `systemd --user` unit that environment is the user manager's, which contains
`WAYLAND_DISPLAY` only if the compositor imported it. If it is absent **and** idle policy
is disabled in `session.kdl` (so `idle::run` returns early at `idle.rs:630–637` without
ever touching Wayland, and E-4's fatal path never triggers), the daemon starts perfectly
happily, holds its inhibitor, receives `PrepareForSleep`, spawns a locker that cannot
connect to a compositor and dies, waits out the full deadline, logs the `Unconfirmed`
warning, and lets the suspend proceed. The journal does record it — the `Unconfirmed`
warning is the only symptom — but nothing at startup would have predicted it. Stage 8's
unit file and README should state the environment requirement explicitly, and the
end-to-end test sequence should verify `loginctl lock-session` actually produces a lock
*with the unit running under systemd*, not only under `cargo run` from a terminal (which
inherits a complete environment and therefore cannot reproduce this).

**I-7 — what the locker inherits, exactly.** `stdin` is `/dev/null` (`sleep.rs:752`) —
correct, the locker reads its password from Wayland. `stdout`/`stderr` stay inherited, so
under systemd they go to the journal, which is where a locker's complaints belong; the
comment saying so is accurate. The child keeps the daemon's uid/gid (both are the user's),
cwd, umask, signal dispositions and — per F-2 — cgroup. No `setsid`, so it also keeps the
process group and session, meaning a `SIGINT` delivered to the daemon's process group by
an interactive `cargo run` under Ctrl-C reaches the locker too. That is harmless during
development (Ctrl-C is a deliberate stop) but it is the same mechanism as F-2 in a
different coat, and `setsid`-ing the locker would close both. Worth considering alongside
F-2's fix.

---

## Checked and found clean

Absence of a finding below means the item was actually examined, with the evidence named.

**Panic and silent-death surface — clean in this crate.** See **I-1** for the full
per-file evidence. Beyond the scan, task-death semantics were traced path by path against
`CLAUDE.md`'s requirement that a module's death kill the process rather than strand a
half-alive daemon, and — with the four exceptions that are findings above (E-1's degraded
sleep latch, L-2's exit-0 path, L-3's mislabelled fatality, L-6's tie-break) — the wiring
does what it claims:

- `main.rs:214–226` `select!`s shutdown against `tasks.join_next()`, and **any** task
  completing before shutdown yields `ExitCode::FAILURE`. `JoinSet::join_next` is
  documented cancel-safe, so the losing branch loses nothing.
- A task *panicking* is covered too, and this is the subtle one: `JoinSet` catches the
  unwind rather than aborting the process, but the resulting `JoinError` still counts as
  a completion, so the same `select!` arm fires and the process still exits non-zero. The
  panic is additionally logged at `main.rs:236–244` when the remaining tasks are joined.
- `sleep::run` returns on any of its three signal streams ending (`:1202`, `:1225`,
  `:1240`), each with a distinct `error!`. (E-2 explains why a logind *restart* does not
  reach these; a dead *bus* does.)
- `idle::run` returns when the Wayland event channel closes (`:710–722`), which is what
  `wayland_thread_main` produces by dropping `event_tx` on any dispatch error
  (`:582–597`). A panic inside the Wayland thread reaches the same place, because
  unwinding drops the sender. This is the one non-tokio thread in the crate and its death
  is correctly plumbed back into the tokio side.
- `inhibit::run`'s three degraded paths (no session bus `:642–654`, canonical-path
  registration failure `:666–683`, name already owned or claim error `:706–712`) each
  park on `shutdown.changed()` and hold `inhibit_tx` alive, which is correct: the channel's
  initial `false` is already `idle::run`'s no-inhibit default, so doing nothing is exactly
  right. The `DBusProxy`-construction failure path (`:716–731`) deliberately parks rather
  than returning, and its stated reason — returning would drop the `#[must_use]`
  `Connection` and silently stop serving `Inhibit`/`UnInhibit` — is correct.
- **Shutdown ordering is genuinely ordered, not raced.** `async_main` sends on the
  shutdown watch and then `join_next()`s every task to completion before returning
  (`main.rs:233–245`), so `sleep::run`'s `machine.release_for_shutdown()` (`:1260`) runs —
  and the fd closes — before the process exits. There is no `std::process::exit` anywhere
  in the crate to short-circuit that. `watch` (rather than a per-task `oneshot`) is the
  right primitive for the fan-out and the doc comment's reasoning about late subscribers
  is correct.

**The sleep state machine's transitions — clean, and they match Architecture's diagram
edge for edge.** `PrepareForSleep(true)` from `Awake` → `LockPending` →
`lock_if_needed(BeforeSleep)` → confirm-or-deadline → **release on every path**
(`sleep.rs:564–567`; there is exactly one `release_inhibitor` call in
`on_going_to_sleep` and it is unconditional, after the `match`) → `ReadyToSleep`.
`PrepareForSleep(true)` from `ReadyToSleep` → `warn!` and no-op (`:521–532`), which is the
duplicate-signal guard Architecture asks for. `PrepareForSleep(false)` → `ensure_inhibitor`
**first**, then the state assignment, then the log (`:577–593`) — the ordering
Architecture calls emphatic, and `resume_reacquires_the_inhibitor_before_anything_else`
asserts it against a shared fake journal rather than against a comment, which is the right
way to pin an ordering constraint. All four `SleepReadiness` variants release. Concurrency
within the task is impossible by construction: `handle_event` takes `&mut self` and each
`select!` branch awaits it to completion, so two events can never race the same inhibitor;
the cost (a shutdown can wait one deadline) is bounded at ~2 s and well inside systemd's
stop timeout, and the doc comment reasons this out correctly. The `Unlock` signal being
deliberately inert (`:492–508`) is the right call and the reasoning — acting on it would
make `loginctl unlock-session` an authentication bypass for anyone on the session bus — is
exactly right; keep it.

**`connect`'s subscribe-then-inhibit ordering — clean, and the reasoning is sound.**
`connect_logind` registers all three signal subscriptions (`sleep.rs:1020–1031`) before
anything takes an inhibitor, and `machine.start()` is not called until `run` reaches
`:1163`. The doc comment's argument (`:937–948`) is correct: inhibit-first would risk
holding a delay nobody releases *and* missing the `PrepareForSleep` that would have made
us lock, whereas subscribe-first risks only losing a delay we never held. There is a
residual window between `connect` returning and `run` being scheduled, but signals
arriving in it are buffered in the zbus streams and handled on the first loop turn, so
nothing is dropped — the exposure in that window is the missing *delay*, not a missing
lock, which is the strictly better of the two.

**`resolve_session_path` — clean, and the `/session/auto` trap is correctly avoided.**
The three-rung chain (`$XDG_SESSION_ID` → `GetSessionByPID` → the user's `Display`
session, `sleep.rs:1063–1112`) resolves a **session-specific** object path, which matters
because `man org.freedesktop.login1` is explicit that `/session/self` and `/session/auto`
never emit signals — a daemon that subscribed there would silently never see `Lock`. The
third rung is the one that will actually be used under Stage 8's `systemd --user` unit
(the daemon lives in `user@.service`, outside any session scope, so rungs 1 and 2 both
miss), and it correctly rejects the `"/"` sentinel logind returns for "no display
session" (`:1102–1106`) rather than building a proxy on a path that cannot work. The two
`#[zbus(name = "GetSessionByPID")]` / `"GetUserByPID"` overrides are necessary and
correct — zbus's pascal-casing would otherwise emit `GetSessionByPid`, which logind does
not implement.

**`CacheProperties::No` on both proxies — clean, and load-bearing.** `sleep.rs:1003` and
`:1014` disable zbus's property cache, so every `locked_hint()` is a real `Get` round
trip. The comment's reasoning is right and worth preserving: zbus's cache is fed by
`PropertiesChanged`, and a cache that missed one edge would mean this daemon believes the
session is locked when it is not — the single belief that must never be wrong here. A
round trip every 50 ms for at most two seconds is a trivial price. The confirmation loop
itself (`:664–696`) is also correct in its details: read failures do **not** abort the
wait (logind can be briefly unavailable while the lock comes up), they are warned once
rather than once per poll via a `warned` latch, and the whole thing is bounded by one
`tokio::time::timeout` rather than by counting iterations.

**The already-locked check within a single task — clean.** `lock_if_needed`'s failure
bias (`sleep.rs:310–322`) is right and consistently applied: an *unreadable* `LockedHint`
counts as **unlocked** and therefore spawns, trading a possible severity-3 spurious lock
for the impossibility of a severity-1 exposure, never the reverse. Within `sleep::run`
the check cannot be raced, for the serialisation reason above. E-3 is specifically and
only about the *cross-task* case.

**"Inhibits gate idle only, never sleep" — clean, and structurally enforced, re-verified
by grep rather than taken from the handoff.** `grep -nE 'sleep|SessionLocker|LockTrigger|
Logind' src/modules/inhibit.rs` returns **exactly one** hit:
`inhibit.rs:75`, `use crate::modules::sleep::BoxFuture;` — a type alias for
`Pin<Box<dyn Future ...>>` and nothing more. No `SessionLocker`, no `Logind`, no
`LockTrigger`, no field, no parameter. `main.rs:203`'s call is
`modules::inhibit::run(inhibit_tx, shutdown_rx.clone())` — no locker handle, unlike
`modules::idle::run` at `:197–202`, which does take one. A future edit that tried to make
an inhibit suppress the before-sleep lock would have to *add* a dependency edge, not flip
a flag. Stage 6's claim holds exactly as written.

**Inhibit-active plumbing — clean, and Stage 6's sixth open question resolves to a
non-issue.** Stage 6 asked whether `idle::run`'s `inhibit_closed` latch (`idle.rs:672`,
`:686`, `:693–700`) could latch against a *stub's* sender and then fail to re-arm for the
real one. It cannot, and the reason is structural rather than lucky: `main.rs` constructs
**exactly one** `watch::channel(false)` for this purpose (`main.rs:187`; `grep -n
'watch::channel' src/main.rs` returns two hits total, the other being the shutdown
channel), and `inhibit_tx` is *moved* into `modules::inhibit::run` at `:203` — there is no
stub left in the tree and no second sender to confuse. The latch can therefore only ever
be set by `inhibit::run` returning, which is already fatal to the process. Additionally,
`Shared::apply` (`inhibit.rs:406–429`) is the **only** `active_tx.send` call site in the
module (both `Inhibit`/`UnInhibit` and the peer-vanish watcher route through it), and the
`BecameActive`/`BecameInactive`/`Unchanged` transition is computed *inside*
`InhibitStore`'s own methods from a before/after `is_active()` comparison rather than
re-derived by callers — so "send only on zero↔nonzero transitions" is correct by
construction rather than by convention, which is the right way to build it.
`a_second_concurrent_inhibit_does_not_resend` pins it.

**Cookie bookkeeping and peer-vanish cleanup — clean (bounds aside; see D-1).**
`InhibitStore` is genuinely pure — no I/O, no clock, no `Arc` — and its nine unit tests
cover grant, distinct-and-increasing cookies, last-release-becomes-inactive,
one-of-several-stays-active, double-`UnInhibit`, unknown-cookie `UnInhibit`,
per-peer-scoped vanish, last-peer vanish, and unrelated-peer vanish. Treating a double or
unknown `UnInhibit` as a silent no-op (`inhibit.rs:199–203`, logged at `debug!` only) is
correct per the freedesktop fire-and-forget cookie model. The `NameOwnerChanged`
subscription is filtered server-side to `(2, "")` — new owner is empty, i.e. an actual
disconnect — so the watcher does not wake for every name change on a busy session bus.
The `std::sync::Mutex` choice over `tokio::sync::Mutex` is correct: no `.await` occurs
inside any guard, and `Shared::apply`'s `send` deliberately happens *after* the guard is
dropped in every caller.

**The name claim — clean, and the one flag that matters is right.**
`ZbusNameClaimant::try_claim` (`inhibit.rs:288–314`) requests
`RequestNameFlags::DoNotQueue` **alone**. This is the load-bearing detail Stage 2 and
Stage 5 identified: niri registers its own shim with `AllowReplacement` set, so a claim
that also set `ReplaceExisting` would *succeed* and silently steal the name from niri,
breaking niri's own `ext-idle-notify-v1` suppression for every other app on the machine —
the exact opposite of this module's purpose. Without `ReplaceExisting`, zbus maps an
already-owned name to `Err(zbus::Error::NameTaken)` regardless of the current owner's
flags, which `decide_service_mode` maps to `Inert`. A claim *error* (a real bus problem,
not "already owned") also resolves to `Inert`, which is the conservative direction —
running inert costs one layer of idle suppression on a machine where niri already provides
it natively, and never touches the sleep path. `unowned_name_is_claimed_and_serves`,
`already_owned_name_stays_inert` and `claim_error_stays_inert` pin all three. Registering
the interface at both `/org/freedesktop/ScreenSaver` and the legacy `/ScreenSaver`
**before** attempting the claim follows `request_name_with_flags`'s own documentation and
closes the window where an inbound call could be routed to an empty object server; the
asymmetric failure handling (canonical path fatal-to-serving, legacy path a warning) is
the right split.

**Config resilience — clean, and the severity ordering is correctly encoded.** The rule
that matters is that no bad-config path can ever resolve `lock_before_sleep` to `false`,
and it holds on every branch: absent node → `true` (`config.rs:255`), present-but-not-a-
bool → `warn!` and `true` (`:258–263`), unparseable document → the whole
`SessionConfig::default()`, whose `lock_before_sleep` is `true` (`:145–153`,
`:210–220`), missing or unreadable file → the same default (`:200–209`). A bad config can
silence *idle* policy (which defaults to disabled anyway, so the fallback is not a
downgrade) but can never silence before-sleep locking. The timeout parser rejects rather
than guesses — a bare `"90"` string has an ambiguous unit against the integer form's
minutes and is refused rather than assumed (`:353–359`, `:369–387`) — and both integer
and string forms are guarded against zero, negatives, floats and overflow. The env-var
chain treats an empty string as unset at every rung, including `HOME`, which is what stops
a `HOME=""` from producing the *relative* path `.config/saola` (`:305–324`). 23 unit
tests, including the missing-file and nonsense-value paths Stage 3's task named.

**No source file was modified.** `git status --porcelain` is byte-identical before and
after this review (` M .gitignore`, ` D LICENSE`, and the same eleven untracked entries).
The only paths this stage created are `docs/REVIEW-v0.1.md` and
`.claude/handoffs/handoff_stage_7.attempt_1.md`, both inside already-untracked
directories.

---

## What Stage 8 must do

**Blocking (must-fix before v0.1):**

1. **F-1** — make an inhibit clearing re-fire a suppressed idle action, instead of waiting
   for keyboard activity that may never come. Split `Arm::Suppressed` from `Arm::Fired`
   and re-evaluate on `InhibitChanged(false)`. Add the test that is currently missing.
2. **F-2** — `KillMode=process` in `contrib/systemd/saola-session.service`, so a daemon
   restart does not kill the lock screen it spawned. Consider the startup
   re-lock-if-`LockedHint` belt as well.
3. **E-1** — stop latching degraded logind mode. Retry `connect_logind` on a timer, and
   keep saying so in the journal while degraded, so `active (running)` stops being a lie.
4. **E-2** — subscribe to `NameOwnerChanged` for `org.freedesktop.login1` and
   release-then-re-acquire the inhibitor when logind restarts. Fold **E-5**'s missing
   background retry into the same loop as E-1's.
5. **E-3** — give `SessionLocker` shared `last_spawn` state so the idle and sleep tasks
   cannot both spawn inside the ~360 ms `LockedHint` lag. Test it with two clones.
6. **E-4** — `RestartSec=2s` and `StartLimitIntervalSec=0` in the unit, plus a retrying
   Wayland connect at startup, so a lost startup race cannot leave the unit permanently
   `failed`.
7. **D-1** — bound the ScreenSaver shim's strings and grant count before shipping a build
   that could ever own the name.

**Deferred (document, don't block):** E-6 (raise the deadline to 4 s and fix the constant's
rationale), L-1 (`--check-config` needs a subscriber — do it, it is three lines and the
README depends on it), L-2, L-3 (correct the comment and the journal line), L-4, L-5
(README note in the nested-testing section), L-6 (`biased;` in `async_main`'s `select!`),
and the README/unit lines called for by I-3 (advisory tooling), I-5, I-6 (the
`WAYLAND_DISPLAY` requirement) and I-7.

**And before tagging 0.1.0:** the Jordan-run end-to-end sequence Stage 8's task list
specifies still has to happen — enable the unit, `systemd-inhibit --list`,
`loginctl lock-session`, a short idle-lock timeout, then a real `systemctl suspend` with
the locker confirmed on screen at resume. Two of this review's findings can only be
falsified there: **E-2**'s stale-fd behaviour (visible as a suspend that does not wait)
and **I-6**'s environment requirement (visible only when running under systemd, never
under `cargo run`). Run that sequence against the *fixed* code, not this one.
