---
project_type: rust
max_retries: 1
on_failure: halt
---

# saola-session — idle, sleep and lock wiring for the Saola DE (v0.1)

## Context

Jordan is building "Saola", a Linux desktop environment in Rust, targeting the
**niri** compositor. Three sibling projects exist and are the convention
sources — mirror them, don't reinvent:

- **saola-lockscreen** (`~/Developer/saola-lockscreen`) — the session locker
  this daemon spawns. Its `CLAUDE.md` and `PLAN.md` are this plan's direct
  templates; its `contrib/session/` directory (a before-sleep shell script +
  `systemd --user` unit + a swayidle recommendation) is the scaffolding **this
  project exists to replace** — its README says so explicitly.
- **saola-panel** (`~/Developer/saola-panel`) — the status bar; source of the
  repo layout (`rust-toolchain.toml`, `rustfmt.toml`, dual MIT/Apache license,
  `release-plz.toml`, `CLAUDE.md` + `AGENTS.md`) and the avoid-heavyweight-deps
  rule.
- **saola-theme** (`~/Developer/saola-theme`) — the design system. **This
  daemon has no UI and therefore, uniquely among Saola components, no
  saola-theme dependency.** It is referenced only for repo conventions.

Goal: **one daemon, one binary crate**, unifying three concerns that share one
state machine (the explicit decision, made with the user, over separate
`saola-idle`/`saola-session` projects — the concerns gate each other, and
splitting them would need an IPC layer whose only job is reassembling the
shared state):

1. **Idle policy** via the Wayland `ext-idle-notify-v1` protocol (niri
   implements it): lock after N minutes, power off outputs after M (via
   `niri msg action power-off-monitors`).
2. **Before-sleep locking** via logind: hold a `sleep` delay inhibitor, catch
   `PrepareForSleep`, spawn the locker, confirm the lock actually took effect,
   release the inhibitor. Also honor logind's session `Lock` signal so
   `loginctl lock-session` works.
3. **An `org.freedesktop.ScreenSaver` D-Bus shim** so apps that inhibit idle
   the freedesktop way (Firefox video is the canonical case) actually suppress
   idle actions. Inhibits gate **idle only, never the before-sleep lock** — a
   lid close mid-movie must still lock.

**Decisions made with the user:**

- Config file `~/.config/saola/session.kdl` — optional, built-in defaults,
  read once at startup, same KDL family and resolution order as the panel's
  `panel.kdl` and the lockscreen's `lockscreen.kdl`.
- The locker command is configurable; default `saola-lockscreen`. The daemon
  must not hardcode more knowledge of the locker than "a command to spawn".
- **Failure severity order (binding, mirrors the lockscreen's):**
  (1) *machine suspends or idles with the session unlocked/exposed*,
  (2) *suspend blocked or delayed indefinitely*, (3) spurious lock. When two
  behaviors conflict, prefer the lower number's safety. Note logind itself
  caps delay inhibitors (`InhibitDelayMaxUSec`, typically ~5 s), so "blocked
  forever" is systemically impossible — design within that cap, don't fight it.
- **The lock-confirmation signal is an open research question** (Stage 2).
  The lockscreen's review (its `docs/REVIEW-v0.1.md`, finding L-3) established
  the locker itself cannot confirm its lock succeeded. The working hypothesis
  is logind's per-session `LockedHint` property, set by the compositor —
  **unverified against niri as of 2026-08-02**. Fallbacks to investigate:
  niri's IPC event stream, then process-liveness + grace period (what the
  retired shell script does). Everything Stage 4 builds keys off Stage 2's
  finding; do not guess.
- Status on the machine (2026-08-02): the lockscreen's PAM path is verified
  working (nested-niri round-trip: wrong password → error + retry, correct
  password → unlock, rosec vault auto-unlock confirmed). Its first **real**-
  session lock is still pending; Stage 2's Jordan-assisted check should
  piggyback on it. **No idle daemon is installed** (`swayidle` was recommended
  to Jordan as an interim — if he installed it, Stage 8's docs must say to
  remove that wiring when enabling this daemon).
- Jordan runs commands needing root himself: stages must never run `sudo`
  (this daemon is user-level throughout, so root should never be needed —
  flag it as a design smell if a stage thinks it is).

**Every stage subagent must first read the Architecture section of this file
(`PLAN.md` at repo root) and `~/Developer/saola-lockscreen/CLAUDE.md`** (the
closest sibling's conventions — the sudo rule, the nested-niri testing rule,
the no-panic rule, the teaching-notes commenting style for a Rust-newer
author all carry over; Stage 1 derives this repo's own `CLAUDE.md` from it).

## Architecture

**Single binary crate** (an app, not a library — no workspace), mirroring the
lockscreen's layout:

```
saola-session/
├── Cargo.toml
├── rust-toolchain.toml         # channel = "stable"
├── rustfmt.toml                # defaults
├── contrib/
│   └── systemd/saola-session.service   # Stage 8 — user unit (user-level, no root)
├── docs/
│   └── SIGNALS.md              # Stage 2 — verified signal research
└── src/
    ├── main.rs                 # tokio event loop: wires modules, owns shutdown
    ├── config.rs               # session.kdl: timeouts, locker command, toggles
    └── modules/
        ├── mod.rs
        ├── sleep.rs            # Stage 4 — logind: inhibitor, PrepareForSleep, Lock signal
        ├── idle.rs             # Stage 5 — ext-idle-notify-v1 policy engine
        └── inhibit.rs          # Stage 6 — org.freedesktop.ScreenSaver shim
```

### The sleep state machine (the safety core — binding)

```
Awake(inhibitor held) ──PrepareForSleep(true)──▶ LockPending(spawn locker,
  ▲                                                await confirmation)
  │                                                   │
  └──PrepareForSleep(false): re-acquire inhibitor──┐  │ confirmed, or deadline
                                                   │  ▼ (whichever first)
                                          ReadyToSleep(inhibitor released)
```

- The inhibitor is a file descriptor from logind's `Inhibit()`; releasing =
  dropping the fd. It is taken at startup and **re-acquired on every resume
  before anything else** — a daemon that sleeps without holding it has
  silently become decorative.
- `LockPending` must resolve by **deadline even if confirmation never
  arrives** (severity rule 2, and logind would force it anyway): if the
  locker was spawned successfully, release and let sleep proceed — the locker
  will be on screen by resume in every non-pathological case; log loudly.
  If the spawn itself failed, still release, log at error level.
- If the session is already locked (per Stage 2's signal) when
  `PrepareForSleep(true)` arrives, skip the spawn — never stack lockers.
- logind's session `Lock` signal → same spawn-if-not-locked path.
- No `panic!`/`unwrap`/`expect` on any runtime path, same rule and same
  clippy enforcement as the lockscreen. A crashed daemon here means the next
  suspend is unprotected — the failure is silent, which is worse than loud.

### Idle policy

Two independent `ext-idle-notify-v1` notifications from config: `lock` (spawn
locker if not already locked) and `power-off` (`niri msg action
power-off-monitors`); each re-arms on the protocol's resume event. An active
ScreenSaver inhibit (Stage 6) suppresses **new** idle actions but never
cancels an in-flight lock and never touches the sleep path. All policy is a
pure state machine over injected events — the Wayland and D-Bus plumbing feed
it, tests drive it directly.

### Testing strategy

- **Traits + fakes over live buses**: `sleep.rs` and `inhibit.rs` abstract
  logind/D-Bus behind small traits (the lockscreen's `Authenticator` pattern);
  the state machines are unit-tested exhaustively with fakes (confirm, timeout,
  spawn-failure, already-locked, inhibit-during-idle variants).
- **Nested niri** (the lockscreen `CLAUDE.md` procedure) exercises the
  *Wayland* side live: short idle timeouts against a nested compositor, hands
  off the nested keyboard. **The logind side cannot be tested nested** — a
  nested niri is not a logind session, has no `LockedHint`, receives no
  `PrepareForSleep`. Real-session checks are Jordan-driven, read-only where
  possible (`busctl get-property`, `systemd-inhibit --list`), and never
  involve an agent running `systemctl suspend`.
- Suspend/resume end-to-end is a Jordan-run test (Stage 8 documents the
  sequence); no stage performs a real suspend itself.

## Stage 1 — Crate skeleton + dependency resolution

```yaml
model: sonnet
effort: low
tools: [Read, Write, Edit, Bash, Glob, Grep]
verify:
  files:
    - Cargo.toml
    - rust-toolchain.toml
    - rustfmt.toml
    - CLAUDE.md
    - src/main.rs
  command: cargo build
```

Read the Architecture section of `PLAN.md`, then
`~/Developer/saola-lockscreen/`'s `Cargo.toml`, `rust-toolchain.toml`,
`rustfmt.toml` and `CLAUDE.md`, and `~/Developer/saola-panel/`'s equivalents.

1. `cargo init` in the existing repo (`.gitignore`/`README.md` exist — keep).
   Replace the single `LICENSE` with the siblings' dual `LICENSE-MIT`/
   `LICENSE-APACHE` pair and matching `Cargo.toml` license field. Mirror
   `rust-toolchain.toml` and `rustfmt.toml` verbatim.
2. Dependencies — survey before pinning, record each choice and why in
   `CLAUDE.md` (the lockscreen's PAM/HTTP survey entries are the format to
   copy): `tokio` (which features does an event-loop daemon actually need —
   start from the lockscreen's minimal `rt`+`sync` stance and justify each
   addition); a D-Bus crate (`zbus` is the expected winner — pure Rust, async,
   tokio-compatible — but survey `dbus-rs` too and record the comparison);
   `wayland-client` + `wayland-protocols` for `ext-idle-notify-v1` (verify
   which crate feature exposes that protocol — it is a staging protocol; note
   the exact feature flag); `kdl` (same version line as the siblings);
   a logging choice (check what the panel and lockscreen do; a daemon wants
   journald-friendly output — survey `tracing` vs plain `eprintln`, record).
   **No iced, no saola-theme** — this crate has no UI (Architecture).
3. `src/main.rs` compiles to a stub that prints version and exits. Empty
   `modules/mod.rs`, stub `config.rs` with a doc comment stating its role.
4. Write this repo's `CLAUDE.md`, derived from the lockscreen's: keep the
   sudo rule, the nested-niri rule (with this repo's logind caveat from
   Architecture's testing strategy), the no-panic rule and its severity
   rationale (silent-failure framing), the commands block, the dependency
   survey entries. Drop everything UI/theme/PAM-specific.

Handoff: exact versions resolved, the D-Bus and logging crates chosen and
why, the `ext-idle-notify-v1` feature flag verified, any surprises.

## Stage 2 — Signal research: how do we know the session is locked?

```yaml
model: sonnet
effort: high
tools: [Read, Write, Edit, Bash, Glob, Grep]
depends_on: [1]
verify:
  files:
    - docs/SIGNALS.md
```

The load-bearing unknown. Everything Stage 4 builds keys off this stage's
findings; wrong answers here become exposure bugs there. Produce
`docs/SIGNALS.md` recording, for each question, the **evidence** (command
output, source permalink, doc quote — not inference):

1. **Does niri set logind's per-session `LockedHint`?** Read niri's source
   (fetch from GitHub — search for `SetLockedHint`/`locked_hint`) and its
   release notes. Then verify empirically: print for Jordan the exact
   read-only check to run during his (still-pending) first real-session
   lockscreen test — `busctl get-property org.freedesktop.login1
   /org/freedesktop/login1/session/auto org.freedesktop.login1.Session
   LockedHint` while locked and after unlocking — and **wait for his
   confirmation of the observed values before writing the conclusion.**
2. **Does niri's IPC event stream expose lock state?** (`niri msg
   event-stream` against the real socket is read-only and safe; also check
   the niri IPC docs/source.) If yes, document the event shape — it may be a
   better primary signal than polling LockedHint, or the fallback if
   LockedHint is unset.
3. **logind mechanics, verified not assumed**: `Inhibit("sleep", …, "delay")`
   call shape and fd semantics; `PrepareForSleep` signal direction and
   ordering; the actual `InhibitDelayMaxUSec` value on Jordan's machine
   (`busctl get-property … org.freedesktop.login1.Manager
   InhibitDelayMaxUSec`); the session `Lock`/`Unlock` signals; what
   `loginctl lock-session` emits. All read-only probes.
4. **`org.freedesktop.ScreenSaver` ownership**: is any process on Jordan's
   session bus already claiming that name (`busctl --user | grep -i screen`,
   plus check what xdg-desktop-portal provides)? Document the coexistence
   story Stage 6 must follow.
5. **`ext-idle-notify-v1` in nested niri**: confirm a nested instance
   advertises the global (`WAYLAND_DISPLAY=… wayland-info | grep idle` or
   equivalent), so Stage 5 knows its live-test plan is viable.

Close with a **decision section**: the primary lock-confirmation signal, the
fallback chain, and the exact mechanism Stage 4 should implement. If evidence
is inconclusive (e.g. Jordan's real-session test hasn't happened yet), say so
explicitly and specify the conservative default (process-liveness + grace
period) plus what would upgrade it.

Handoff: the decision section, verbatim, plus anything surprising about the
probes themselves.

## Stage 3 — Config + daemon scaffold

```yaml
model: sonnet
effort: medium
tools: [Read, Write, Edit, Bash, Glob, Grep]
depends_on: [2]
verify:
  files:
    - src/config.rs
    - src/modules/mod.rs
  command: cargo build && cargo clippy --all-targets -- -D warnings && cargo test
```

Read Architecture, Stage 2's handoff, and the lockscreen's `src/config.rs`
(copy its parse style — hand-walked KDL document, no serde derive — and its
missing-file-means-defaults behavior exactly).

1. `config.rs`: parse `~/.config/saola/session.kdl` — `idle { lock-after
   <minutes>; power-off-after <minutes> }` (either omittable; omitted = that
   action disabled), `locker "command"` (default `saola-lockscreen`),
   `lock-before-sleep <bool>` (default true). Same resolution order as the
   siblings. Unit-test with fixture strings including the missing-file path
   and a nonsense-values path (reject cleanly, fall back to defaults —
   severity rule 1 beats strictness: a bad config must not mean "no locking").
2. `main.rs`: the tokio event loop skeleton — parse config, initialize
   logging (Stage 1's choice, journald-friendly), spawn module tasks (stubs),
   clean shutdown on SIGTERM/SIGINT (systemd stops the unit this way; the
   inhibitor fd must drop cleanly). Comment the async ownership as teaching
   notes, per the siblings' convention.
3. A `--version`/`--check-config` CLI surface (print parsed effective config
   and exit) — cheap, and Stage 8's docs will point at it.

Handoff: config schema as built (Stage 8's README reuses it verbatim), the
shutdown wiring, where module tasks plug in.

## Stage 4 — Before-sleep + lock-signal module (the safety core)

```yaml
model: opus
effort: high
tools: [Read, Write, Edit, Bash, Glob, Grep]
depends_on: [3]
verify:
  files:
    - src/modules/sleep.rs
  command: cargo build && cargo clippy --all-targets -- -D warnings && cargo test
```

The sleep state machine in Architecture is the binding spec; read it first,
plus Stage 2's decision section — implement the confirmation mechanism it
chose, not the one that seems cleanest today.

1. `modules/sleep.rs`: logind connection behind a small trait (the
   lockscreen's `Authenticator` pattern — the state machine takes the trait,
   tests take the fake): take the delay inhibitor at startup; on
   `PrepareForSleep(true)` run the `LockPending` sequence (skip spawn if
   already locked per Stage 2's signal; await confirmation; hard deadline
   from config or a sane constant well under the machine's
   `InhibitDelayMaxUSec` from Stage 2); release by dropping the fd; on
   `PrepareForSleep(false)` re-acquire **first**. Honor the session `Lock`
   signal (spawn-if-not-locked) and `Unlock` if Stage 2 found it meaningful.
2. Locker spawning: `tokio::process`, detached — the daemon never waits for
   the locker to exit, never parses its output, and a spawn failure logs at
   error level and still releases by deadline (severity rules 1 vs 2:
   exposure beats blocked-suspend, but logind's cap means holding forever
   isn't even an option — log loudly and proceed).
3. Unit tests are the stage's centre of gravity: confirmation-arrives,
   confirmation-times-out, spawn-fails, already-locked-skips-spawn,
   resume-re-acquires-before-anything, double `PrepareForSleep(true)`
   (logind restarts exist), `Lock`-signal-while-locked. Every test drives the
   machine through the trait fakes — no D-Bus in tests.
4. Live check (Jordan-driven, no suspend): with the daemon running under
   `cargo run` in his real session, `systemd-inhibit --list` must show the
   delay inhibitor with this daemon's name and why-string;
   `loginctl lock-session` must bring up the locker. Print the commands and
   wait for his confirmation before reporting done.

Handoff: the trait signature, the confirmation mechanism as actually
implemented, test inventory, Jordan's live-check result, anything zbus did
that surprised you.

## Stage 5 — Idle policy module

```yaml
model: sonnet
effort: medium
tools: [Read, Write, Edit, Bash, Glob, Grep]
depends_on: [4]
verify:
  files:
    - src/modules/idle.rs
  command: cargo build && cargo clippy --all-targets -- -D warnings && cargo test
```

Read Architecture (idle policy block), Stage 2's handoff (nested-niri
idle-notify viability, lock-state signal), Stage 3's config schema, and the
lockscreen `CLAUDE.md`'s nested-niri procedure.

1. `modules/idle.rs`: register the configured `ext-idle-notify-v1`
   notifications (skip entirely when config disables both — don't hold a
   Wayland connection for nothing); on `lock` timeout, spawn-if-not-locked
   (reuse Stage 4's spawn path — one implementation, not two); on
   `power-off` timeout, `niri msg action power-off-monitors` (via
   `tokio::process`, failure logged and otherwise ignored — severity rule 3);
   re-arm on resume events.
2. The policy core is a pure state machine over injected events (notify,
   resume, inhibit-active, config) — unit-test it directly: each timeout
   fires its action once, resume re-arms, inhibit suppresses new actions but
   not in-flight ones, disabled actions never fire.
3. Live test in a **nested niri** (lockscreen `CLAUDE.md` procedure): 5-second
   timeouts in a throwaway config, hands off the nested window, watch the
   locker appear inside it; unlock via the locker's dev-unlock Escape. The
   nested instance has no logind session — expect and tolerate the sleep
   module's logind side being degraded there (it must log-and-continue, not
   crash: that behavior is itself part of this test).

Handoff: what the nested test showed (including how the daemon behaved
without logind), power-off verification notes, any idle-notify quirks.

## Stage 6 — ScreenSaver inhibit shim

```yaml
model: sonnet
effort: medium
tools: [Read, Write, Edit, Bash, Glob, Grep]
depends_on: [5]
verify:
  files:
    - src/modules/inhibit.rs
  command: cargo build && cargo clippy --all-targets -- -D warnings && cargo test
```

Read Architecture (inhibits gate idle only — binding), Stage 2's handoff
(name-ownership findings), Stage 5's handoff (how idle consumes the
inhibit-active flag).

1. `modules/inhibit.rs`: serve `org.freedesktop.ScreenSaver` on the session
   bus per Stage 2's coexistence findings (own the well-known name only if
   nothing else does; if something does, log and run degraded rather than
   fight over it). Implement `Inhibit(app, reason) -> cookie` and
   `UnInhibit(cookie)`; **track each inhibitor's bus peer and drop its
   cookies when the peer vanishes** (apps crash without UnInhibiting — a
   leaked cookie is a permanent "never lock on idle", severity rule 1).
2. Expose "any active inhibits?" to the idle module the way Stage 5's handoff
   expects; never consult it on the sleep path.
3. Unit tests: cookie grant/release, peer-vanish cleanup, double-UnInhibit,
   inhibit-state visibility. D-Bus behind the same trait-and-fake pattern.
4. Live check: with the daemon running (nested or real session — this module
   doesn't care), `busctl --user call` an Inhibit, confirm idle actions stop
   arming (log line), UnInhibit, confirm re-arm. Fully user-level and
   read-only beyond the daemon's own bus traffic.

Handoff: name-ownership behavior as shipped, cookie semantics, what Firefox
actually does when you play a video (test it if convenient — it is the whole
point of the module).

## Stage 7 — Review: exposure and silent-failure audit (read-only)

```yaml
model: opus
effort: high
tools: [Read, Grep, Glob, Bash]
depends_on: [6]
verify:
  files:
    - docs/REVIEW-v0.1.md
```

Read-only adversarial review of the whole crate against Architecture's
severity order; the lockscreen's `docs/REVIEW-v0.1.md` is the format and
rigor bar. Bash is for `cargo clippy`/`cargo test`/`cargo tree` and
read-only inspection — make NO source edits; findings go in the report for
Stage 8.

Audit at minimum:

1. **Exposure paths (severity 1)**: every way the machine can reach suspend
   without the locker spawned — inhibitor not held (startup ordering, resume
   re-acquire, D-Bus reconnect after logind restart), `PrepareForSleep`
   missed, deadline released before spawn confirmed, already-locked check
   wrong. Every way idle-lock can silently stop: leaked inhibit cookies,
   un-re-armed notifications, dead Wayland connection.
2. **Silent-death surface**: the daemon's failure mode is quiet absence.
   Panic surface (`unwrap`/`expect`/indexing/arithmetic on runtime paths,
   clippy-verified), task-death handling (does one module's death kill the
   process — it should, so systemd restarts it — or strand a half-alive
   daemon?), and whether the unit file's restart policy assumption matches.
3. **Spawn hygiene**: locker spawn cannot block the event loop; no output
   parsing; no double-spawn windows between idle and sleep paths racing.
4. **D-Bus surface**: the shim is an unauthenticated session-bus service —
   what can a hostile peer do (cookie floods, giant strings, rapid
   inhibit/uninhibit)? Bounded memory, no amplification.
5. **Dependency review**: `cargo tree -e normal` for surprises; flag
   anything unmaintained or duplicated.

Write `docs/REVIEW-v0.1.md`: findings ordered by severity with file:line and
a concrete fix sketch; explicitly state what was checked and found clean —
absence of findings must be evidence of review, not of skipping.

## Stage 8 — Fixes, packaging, docs, release prep

```yaml
model: sonnet
effort: medium
tools: [Read, Write, Edit, Bash, Glob, Grep]
depends_on: [7]
verify:
  files:
    - contrib/systemd/saola-session.service
    - README.md
    - CHANGELOG.md
  command: cargo build && cargo clippy --all-targets -- -D warnings && cargo test
```

Read `docs/REVIEW-v0.1.md` first and fix every must-fix finding (document
deliberate won't-fixes with reasoning in the changelog entry). Then:

1. `contrib/systemd/saola-session.service`: a `systemd --user` unit —
   `Restart=on-failure` (Stage 7 audited the daemon's death semantics against
   exactly this), correct ordering (`After=graphical-session.target`,
   `PartOf=`), install instructions being user-level `install -Dm644` +
   `systemctl --user enable --now` (no root anywhere).
2. **Retire the lockscreen's interim wiring**: README section with the exact
   commands for Jordan — disable/remove `saola-lock-before-sleep.service` and
   any swayidle autostart line — plus a PR-ready note for
   `saola-lockscreen/contrib/session/README.md` pointing here (write the
   note's text in this repo's docs; do NOT edit the sibling repo).
3. `README.md`: what it is, the three concerns and the severity order,
   build/install, full `session.kdl` reference (Stage 3's schema),
   `--check-config`, the niri `spawn-at-startup` line, known limitations
   (whatever Stage 2 concluded about lock confirmation, the logind
   nested-testing gap), credits to the sibling repos.
4. `CHANGELOG.md` + `release-plz.toml` mirroring the panel's setup; version
   stays `0.1.0-dev` — tagging is Jordan's call after the end-to-end
   suspend/resume test.
5. Write the **Jordan-run end-to-end test sequence** into the README (this
   plan's stages never suspend the machine): enable the unit; verify
   `systemd-inhibit --list`; `loginctl lock-session`; idle-lock with a short
   timeout; then a real `systemctl suspend` with the locker confirmed on
   screen at resume — and only after that, tag.

Handoff: findings fixed vs deferred, the exact commands Jordan runs (unit
enablement, interim-wiring retirement), and the end-to-end sequence's
expected observations.
