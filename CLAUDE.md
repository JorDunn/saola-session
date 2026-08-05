# saola-session — agent instructions

Idle, sleep and lock wiring for Saola, a Linux desktop environment built in Rust.
Closest sibling and convention source: [saola-lockscreen](https://github.com/JorDunn/saola-lockscreen)
(the session locker this daemon spawns) — this file is derived from its `CLAUDE.md`.
Repo-layout conventions (`rust-toolchain.toml`, `rustfmt.toml`, dual MIT/Apache
license) come from [saola-panel](https://github.com/JorDunn/saola-panel). Target
compositor: **niri** (`ext-idle-notify-v1` for idle, logind for sleep/lock).

**This daemon has no UI and, uniquely among Saola components, no `saola-theme`
dependency** — it never draws anything. Full context and the decisions behind this
crate live in this repo's `PLAN.md` — its **Architecture** section is binding and
every stage subagent must read it before making changes; this file is the second
required read.

## Commands

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings   # CI gate — keep it green
cargo fmt --check                            # CI gate
cargo run                                    # real session: takes a sleep
                                              # inhibitor, opens a real Wayland
                                              # connection — see the nested-niri
                                              # testing rule below before running
                                              # anything that touches the Wayland
                                              # side
cargo run -- --check-config                  # Stage 3: print effective config, exit
```

## Architecture

Single binary crate (an app, not a library — no workspace), mirroring the
lockscreen's layout:

```
src/
├── main.rs                 # tokio event loop: wires modules, owns shutdown
├── config.rs                # session.kdl: timeouts, locker command, toggles
└── modules/
    ├── mod.rs
    ├── sleep.rs              # Stage 4 — logind: inhibitor, PrepareForSleep, Lock signal
    ├── idle.rs               # Stage 5 — ext-idle-notify-v1 policy engine
    └── inhibit.rs            # Stage 6 — org.freedesktop.ScreenSaver shim
```

One daemon, one binary, unifying three concerns that share one state machine — the
explicit decision, made with Jordan, over separate `saola-idle`/`saola-session`
projects (the concerns gate each other; splitting them would need an IPC layer whose
only job is reassembling the shared state):

1. **Idle policy** via `ext-idle-notify-v1` (niri implements it): lock after N
   minutes, power off outputs after M (`niri msg action power-off-monitors`).
2. **Before-sleep locking** via logind: hold a `sleep` delay inhibitor, catch
   `PrepareForSleep`, spawn the locker, confirm the lock took effect, release the
   inhibitor. Also honors logind's session `Lock` signal (`loginctl lock-session`).
3. **An `org.freedesktop.ScreenSaver` D-Bus shim** so apps that inhibit idle the
   freedesktop way (Firefox video is the canonical case) actually suppress idle
   actions. Inhibits gate **idle only, never the before-sleep lock** — a lid close
   mid-movie must still lock.

### Failure severity order (binding — the rule everything else in this file serves)

1. Machine suspends or idles with the session unlocked/exposed.
2. Suspend blocked or delayed indefinitely.
3. Spurious lock.

When two behaviors conflict, prefer the lower number's safety. logind itself caps
delay inhibitors (`InhibitDelayMaxUSec`, typically ~5 s), so "blocked forever" is
systemically impossible — design within that cap, don't fight it.

### The sleep state machine (the safety core — binding)

```
Awake(inhibitor held) ──PrepareForSleep(true)──▶ LockPending(spawn locker,
  ▲                                                await confirmation)
  │                                                   │
  └──PrepareForSleep(false): re-acquire inhibitor──┐  │ confirmed, or deadline
                                                   │  ▼ (whichever first)
                                          ReadyToSleep(inhibitor released)
```

- The inhibitor is a file descriptor from logind's `Inhibit()`; releasing = dropping
  the fd. Taken at startup and **re-acquired on every resume before anything else** —
  a daemon that sleeps without holding it has silently become decorative.
- `LockPending` must resolve by **deadline even if confirmation never arrives**
  (severity rule 2, and logind would force it anyway): if the locker was spawned
  successfully, release and let sleep proceed, log loudly. If the spawn itself
  failed, still release, log at error level.
- If the session is already locked when `PrepareForSleep(true)` arrives, skip the
  spawn — never stack lockers.
- The lock-confirmation signal itself is Stage 2's research question — see
  `docs/SIGNALS.md` once it exists; do not guess at it.

### Idle policy

Two independent `ext-idle-notify-v1` notifications from config: `lock`
(spawn-if-not-locked, reusing the sleep module's spawn path — one implementation,
not two) and `power-off` (`niri msg action power-off-monitors`); each re-arms on the
protocol's resume event. An active ScreenSaver inhibit suppresses **new** idle
actions but never cancels an in-flight lock and never touches the sleep path. All
policy is a pure state machine over injected events — the Wayland and D-Bus plumbing
feed it, tests drive it directly.

## The no-panic rule (binding, severity-critical)

**No `panic!`/`unwrap`/`expect` on any runtime path**, clippy-enforced (the `cargo
clippy --all-targets -- -D warnings` gate above). This is not the lockscreen's
"user gets locked out" framing — it's stricter in a different direction. This
daemon's failure mode is **silent absence**: a crashed lockscreen leaves niri's own
lock surface on screen (its safety net); a crashed `saola-session` leaves the machine
about to suspend with no inhibitor held, or idling with a lock action that will never
fire again, and nothing on screen says so. Systemd's `Restart=on-failure` (Stage 8)
covers a clean process death, but only if the death is prompt and total — a task that
panics inside a `tokio::spawn` without taking the process down, or a Wayland
connection that goes quietly dead without the daemon noticing, is worse than a crash:
it *looks* running. Every module's task death should kill the process, not strand a
half-alive daemon limping along missing one of its three concerns.

## The sudo rule (binding)

**No stage or agent working in this repo ever runs `sudo`, or any command that needs
root.** This daemon is user-level throughout (`systemd --user`, the session D-Bus,
`ext-idle-notify-v1` as the logged-in user) — root should never be needed anywhere in
it; if a stage thinks it needs `sudo`, that is a design smell to flag, not a command
to run. The one system-level artifact this repo produces, `contrib/systemd/
saola-session.service` (Stage 8), is a *user* unit — its own install instructions are
`systemctl --user enable --now`, never anything under `/etc`.

## The nested-niri testing rule (binding, with this repo's logind caveat)

Live Wayland-side testing follows the lockscreen's nested-niri procedure — see its
`CLAUDE.md` for the full step-by-step (spawn `niri -c /tmp/nested-niri.kdl &` without
`--session`, override `NIRI_SOCKET` explicitly since the shell's own points at the
*real* outer niri, run the daemon against the nested `WAYLAND_DISPLAY` only, tear
down when done). That procedure exercises `idle.rs` (Stage 5): short idle timeouts
against the nested compositor, watching the locker spawn inside it.

**The logind side cannot be tested nested — this is this repo's caveat on top of the
lockscreen's rule.** A nested niri is not a logind session: it has no
`LockedHint`, receives no `PrepareForSleep`, and `loginctl lock-session` has nothing
nested to target. `sleep.rs` (Stage 4) must therefore **log-and-continue when its
logind side is degraded in a nested test, never crash** — a nested run exercising
`idle.rs` while `sleep.rs` sits inert for lack of a real session is expected and
correct, not a bug to chase. Real-session logind checks are **Jordan-driven,
read-only where possible** (`busctl get-property`, `systemd-inhibit --list`,
`journalctl --user -u saola-session`), and **no stage or agent ever runs
`systemctl suspend`** — suspend/resume end-to-end is a Jordan-run test Stage 8
documents the sequence for, never something an agent triggers itself.

## Testing strategy (binding — see Architecture and PLAN.md's per-stage detail)

- **Traits + fakes over live buses**: `sleep.rs` and `inhibit.rs` abstract
  logind/D-Bus behind small traits (the lockscreen's `Authenticator` pattern); the
  state machines are unit-tested exhaustively with fakes (confirm, timeout,
  spawn-failure, already-locked, inhibit-during-idle variants) — no D-Bus in tests.
- **Nested niri** exercises the Wayland side live, per the rule above.
- Suspend/resume end-to-end is Jordan-run only; no stage performs a real suspend.

## Conventions

- Jordan is newer to Rust: comment the non-obvious (async task ownership, the
  synchronous-Wayland-to-async bridge, the state machines' trait/fake boundary) as
  teaching notes; prefer explicit code over clever abstraction.
- Copy the established module pattern for new modules (read an existing sibling's
  first): a small trait for the external dependency (D-Bus, Wayland), a pure state
  machine over injected events, unit tests driving the machine through a fake.
- **Dependency survey (2026-08-02, Stage 1) — see `Cargo.toml` for the full
  reasoning inline; summary here for quick reference:**
  - **`tokio`** — `rt-multi-thread`, `macros`, `signal`, `time`, `sync`, `process`.
    Unlike the siblings' minimal `rt` + `sync` (they get a runtime for free from
    iced's own `tokio` feature and only ever `spawn_blocking`), this daemon *is*
    the runtime owner — it needs to build one and run several independent
    long-lived tasks concurrently, hence `rt-multi-thread` over `current_thread`.
  - **D-Bus: `zbus`, not `dbus-rs`.** `dbus-rs` binds the system `libdbus` C
    library (needs its headers/pkg-config at build time, sync-first with async
    bolted on via a separate `futures` feature stack); `zbus` is pure Rust, native
    async, and already what `saola-panel` depends on
    (`default-features = false, features = ["tokio"]`) — matching it means every
    Saola component that talks to the session bus does so through the same wire
    implementation.
  - **`wayland-protocols` needs `features = ["staging", "client"]`** to expose
    `ext-idle-notify-v1` — it's a staging protocol, not in the stable set. Verified
    directly (not just read off docs): with those features, the types resolve at
    `wayland_protocols::ext::idle_notify::v1::client::{ext_idle_notifier_v1::
    ExtIdleNotifierV1, ext_idle_notification_v1::ExtIdleNotificationV1}`.
  - **`kdl = "6.7.1"`** — same version line as both siblings, same hand-walked
    parse style (no serde derive).
  - **Logging: `tracing` + `tracing-subscriber`, not the siblings' `eprintln!`.**
    Both siblings are interactive iced apps with someone watching the screen, where
    a bare `eprintln!` is one line among others a person just saw happen. This is a
    headless `systemd --user` daemon — the severity order above has to survive into
    `journalctl` as levels an operator can filter on after the fact, which
    `error!`/`warn!`/`info!`/`debug!` give and undifferentiated stderr lines don't.
    `tracing-subscriber`'s `fmt` layer writes to stderr (systemd already journals
    that for every unit — no `tracing-journald` direct-socket dependency needed);
    `env-filter` gives `RUST_LOG`-driven verbosity without a recompile.
  - **No `iced`, no `saola-theme`** — this crate has no UI (Architecture, above).
