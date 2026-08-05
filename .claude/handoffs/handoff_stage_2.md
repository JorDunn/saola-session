# Stage 2 handoff — signal research

Full evidence, all five research questions, and the complete decision
section live in `docs/SIGNALS.md` (668+ lines, every claim sourced to
command output, a niri source permalink at commit `8ed0da44` = the exact
`niri --version` build id installed on this machine, or a verbatim doc
quote). This handoff is the compressed version for Stage 3/4 readers: the
decision verbatim, plus what was surprising about the probes themselves.

## Decision section (verbatim from `docs/SIGNALS.md`)

### Primary signal: logind's per-session `LockedHint` — CONFIRMED

**Confirmed, not merely a hypothesis**, by source evidence (§1) plus a
direct real-session observation (§1's "CONFIRMED — Jordan's real-session
observation" table, collected 2026-08-02 with Jordan present at the
machine): `saola-session`'s `sleep.rs` treats `Session.LockedHint == true`
as "the session is locked" for the already-locked-skip-spawn checks in
Architecture's state machine (both the `PrepareForSleep(true)` path and the
logind `Lock`-signal path).

Why this is the right primary signal:

- It's **set by niri itself**, using the exact API (`SetLockedHint`)
  logind's own docs say is "intended to be used by the desktop environment"
  for this purpose (§3) — not a repurposed side-channel.
- It only flips `true` once niri's lock state reaches `Locked(_)` — i.e.
  lock surfaces confirmed up, not merely "locking" — which is precisely the
  confirmation signal `REVIEW-v0.1.md` finding L-3 says the lockscreen
  crate itself cannot provide (it discards the `ext_session_lock_v1`
  `locked`/`finished` events). niri's `update_locked_hint` effectively does
  the confirmation-watching the locker skipped, one layer up, using
  information only the compositor has.
- It requires no new dependency beyond what Stage 1 already chose (`zbus`,
  already pinned) — it's just another property read/watch on a connection
  saola-session needs anyway for `Inhibit`/`PrepareForSleep`/`Lock`.
- **Directly observed**: 468 consecutive polls of `b true` across ~5
  minutes of a real lock, transitioning to `b true` the same second as
  niri's own `locking session` log line, and back to `b false` after a real
  password unlock, with zero `SetLockedHint`-failure warnings in the
  journal across the whole window. This is no longer ambient/historical
  correlation — it's a controlled, time-correlated, Jordan-witnessed
  measurement.

### Stage-4-relevant timing note: lock confirmation is fast, but the deadline stays mandatory

The confirmed observation measured **spawn→compositor-lock ≈ 46ms and
spawn→`LockedHint`-observed ≈ 360ms** (poll-granularity-bounded — the
poller sampled every ~0.57s). An earlier pass through this same data
misread an orchestrator-side delay (monitor armed at 21:46:06, but the
spawn command itself wasn't issued until 21:46:41 — message
composition/tool round-trip time in the orchestrating session) as if it
were locker startup latency, and wrote it up as "~35s cold-start latency."
**That figure was wrong and has been corrected in `docs/SIGNALS.md`** —
flagging it here in case it propagated anywhere else. The locker is
effectively instant, consistent with the lockscreen's own H-1 review fix
(wallpaper decode moved off the pre-lock path).

Corrected implication for Stage 4: on this warm system, `LockPending`
confirmation arrives well inside the 5s `InhibitDelayMaxUSec` window — the
deadline-then-proceed path is **not** the routine outcome. That said,
Architecture's deadline design is still mandatory, not optional: one
warm-system sample doesn't bound worst case (cold caches, resume-time
PAM/vault stalls, disk pressure). Implement the deadline as specified, and
log spawn-to-confirmation duration on every cycle so this stays a
measurement rather than a one-time assumption.

### Fallback chain (in order)

1. **`Session.Lock`/`Session.Unlock` D-Bus signals** (§3) — not a
   lock-*state* signal (they're momentary "asked to lock/unlock" events,
   not a queryable property), but confirmed to be the mechanism
   `loginctl lock-session` drives and confirmed niri does **not** already
   consume them (§3) — so `sleep.rs` must subscribe to these regardless of
   the state signal, to implement the "honor `Lock` signal" bullet in
   Architecture. This is a required second, event-driven listener running
   alongside `LockedHint` polling/watching, not a fallback for state itself.
2. **Process-liveness + grace period** (Architecture's named conservative
   default) — now demoted to a **defensive fallback only**, kept in the
   design in case `LockedHint` ever misbehaves in a scenario this
   observation didn't cover (e.g. a future locker version, a logind
   restart mid-lock, D-Bus reconnect races), not as the primary mechanism.
   It's exactly what the retired shell script did and what
   `REVIEW-v0.1.md`/L-3 says is the only signal available to a *wrapper*
   process that doesn't control the compositor — strictly worse than
   `LockedHint` (doesn't confirm the lock surface is actually up, only that
   the process didn't immediately crash), so Stage 4 should treat it as a
   belt-and-suspenders safety net, not co-equal with the primary signal.

### What would still change this

Nothing outstanding for the base signal choice — it's confirmed. What
*would* warrant revisiting: a future `saola-lockscreen` change that
regresses its startup latency materially (the ~46ms/~360ms numbers above
are one warm-system sample, not a contract), or evidence from Stage 4's own
live check
(`systemd-inhibit --list` / `loginctl lock-session`, per Architecture's
testing strategy) that `LockedHint` behaves differently under the real
sleep-inhibitor path than it did under this manual observation.

### What Stage 4 should NOT do

Do not implement a bespoke "wait for niri's `ext_session_lock_v1` `locked`
event" path inside saola-session — that event belongs to the *locker*
process (`sessionlockev` crate, per `REVIEW-v0.1.md` L-3), not to a
sibling daemon; saola-session has no `ext-session-lock-v1` client role in
Architecture and adding one only to read that event would duplicate the
locker's own unaddressed gap rather than route around it via logind, which
already exists for exactly this cross-process signaling purpose.

## Other confirmed logind mechanics Stage 4 needs (from `docs/SIGNALS.md` §3)

- `Inhibit(what, who, why, mode) -> fd` (types `ssss -> h`); release = drop
  the fd, no explicit release call.
- `PrepareForSleep(bool)` is emitted **by** logind (not called): `true`
  right before sleep, `false` right after resume.
- **`InhibitDelayMaxUSec = 5,000,000 µs (5s)` on this machine** — confirmed
  by `busctl get-property`, not assumed from the "~5s" note in CLAUDE.md.
  Stage 4's `LockPending` deadline must sit comfortably under this (2-3s
  suggested); per the timing note above, confirmation is expected to beat
  it comfortably on the warm-system happy path, but the deadline must still
  be implemented for the pathological cases it wasn't measured against.
- Session `Lock`/`Unlock` are both methods and signals; `loginctl
  lock-session` → `Session.Lock()` → logind re-emits the `Lock` **signal**
  on that session's specific object path (never `/session/self` or
  `/session/auto` — those never emit signals, man-page-confirmed).
  **niri does not itself listen for this signal** (verified by source
  grep) — `sleep.rs` is the thing that must, exactly as Architecture
  specifies.

## Surprises for the probes themselves

1. **niri already owns `org.freedesktop.ScreenSaver` and already wires it
   into its own `ext-idle-notify-v1` suppression** (`docs/SIGNALS.md` §4).
   `busctl --user status org.freedesktop.ScreenSaver` resolves to PID 1336
   = niri itself, not a portal. Its `Inhibit`/`UnInhibit`/peer-vanish-
   cleanup implementation (`src/dbus/freedesktop_screensaver.rs`, read in
   full) feeds `is_fdo_idle_inhibited`, which `refresh_idle_inhibit`
   (`niri.rs:4006-4017`) ORs with native `zwp_idle_inhibit_manager_v1`
   surface inhibits and passes to `idle_notifier_state.set_is_inhibited`
   — suppressing `ext-idle-notify-v1` notifications **inside niri, before
   any client (including this daemon's Stage 5 idle module) ever sees
   them**. Practical effect: the Firefox-inhibits-idle scenario
   Architecture names as Stage 6's reason to exist is already handled for
   free by niri, for anything consuming `ext-idle-notify-v1` — which Stage
   5 already commits to doing. **Stage 6 must not attempt to claim
   `org.freedesktop.ScreenSaver`** — niri set
   `RequestNameFlags::AllowReplacement`, so a naive `request_name` with
   `ReplaceExisting` would actually *succeed* and then silently break
   niri's own suppression for every other consumer (apps would inhibit the
   new owner instead of niri, and `is_fdo_idle_inhibited` would go stale).
   This is a scope question for Stage 6 to resolve with Jordan/the
   orchestrator before that stage starts writing `inhibit.rs` — the module
   may end up being "verify niri's suppression is sufficient" rather than
   "build a competing shim." Full code excerpts in `docs/SIGNALS.md` §4.
2. **A "~35s locker cold-start latency" figure was initially reported and
   was wrong** — it was orchestrator-side round-trip delay, not locker
   startup time (see the timing note above). The corrected measurement is
   spawn→compositor-lock ≈ 46ms, spawn→`LockedHint`-observed ≈ 360ms; the
   locker is effectively instant. Noted here specifically so this
   correction doesn't get lost if only this handoff (not the full
   `docs/SIGNALS.md` history) gets read later.
3. **Live interim wiring is still active on this machine right now**:
   `swayidle -w timeout 300 saola-lockscreen` (PID 1415) and the
   lockscreen contrib's `systemd-inhibit --what=sleep --mode=delay
   --who=saola-lockscreen … saola-lock-before-sleep --once` unit are both
   running — this is exactly the `contrib/session/` scaffolding this
   daemon exists to replace (PLAN.md's opening context). Both must be
   retired in Stage 8's docs; not a Stage 2-4 concern but worth carrying
   forward so Stage 8 doesn't have to rediscover it.
4. niri's `LockedHint` update runs on its own spawned OS thread per
   transition (not synchronous with the redraw loop that triggers it) —
   irrelevant to how saola-session reads the property (plain D-Bus read)
   but noted in case timing ever looks odd during Stage 4 debugging.
5. niri's IPC event stream (`niri msg event-stream`) and its `Event` enum
   (18 variants, exhaustively enumerated) contain **no lock-state
   information at all** — ruled out entirely as a signal, not merely
   deprioritized. Don't reach for it later as a "maybe better" option.

## Pointers for Stage 3/4

- Full evidence, tables, and all five questions: `docs/SIGNALS.md`.
- `docs/SIGNALS.md` §3 has the full `Inhibit`/`PrepareForSleep`/`Lock`
  D-Bus shapes with `busctl introspect` output and man-page quotes — copy
  the method/signal signatures from there rather than re-deriving them.
- `docs/SIGNALS.md` §5 confirms `ext_idle_notifier_v1 v2` is advertised in
  a nested niri instance (Stage 5's live-test plan is viable) using a
  throwaway Rust `wl_registry` lister built against the same
  `wayland-client = "0.31.15"` Stage 1 already pinned — no new tooling
  dependency needed, `wayland-info` itself was never actually needed.
