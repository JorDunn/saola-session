# Stage 3 handoff — config + daemon scaffold

## Config schema as built (`src/config.rs`, `~/.config/saola/session.kdl`)

```kdl
idle {
    lock-after 5            // bare integer = minutes; omit to disable idle-lock
    power-off-after "90s"   // or a quoted duration with unit: "30s", "5m"
}
locker "saola-lockscreen"   // default; any command to spawn
lock-before-sleep #true     // default
```

> **Amended after Stage 5 (Jordan's request, 2026-08-03)**: the two idle
> timeouts now also accept a quoted string with an explicit `s`/`m` unit
> suffix, making sub-minute values expressible (the original bare-integer
> form is still whole minutes and still works). A quoted number with no
> suffix (`"90"`) is deliberately rejected — the unit would be ambiguous.
> Zero, negatives, floats, unknown suffixes all warn and fall back to
> disabled, per the existing per-knob resilience rule.

Top-level nodes, no wrapper (`idle { }`, `locker`, `lock-before-sleep` all sit
directly in the document — unlike the lockscreen's `lockscreen { }` envelope).

- **`SessionConfig`**: `idle: IdleConfig`, `locker: String`,
  `lock_before_sleep: bool`.
- **`IdleConfig`**: `lock_after: Option<Duration>`,
  `power_off_after: Option<Duration>` (renamed from `lock_after_minutes`/
  `power_off_after_minutes: Option<u32>` in the same amendment). `None` =
  disabled, independently per field. No non-`None` default for either —
  idle policy is opt-in.
- **`locker`**: defaults `"saola-lockscreen"`. Stored as a plain `String`
  command line, unvalidated beyond "non-empty after trim" — Stage 4/5 own
  splitting it into argv for `tokio::process::Command` (not done here;
  Architecture: "the daemon must not hardcode more knowledge of the locker
  than 'a command to spawn'").
- **`lock_before_sleep`**: defaults `true`. This is the field the whole
  module doc comment is organized around — every fallback path (missing
  file, garbage document, wrong-typed knob) resolves it to `true`. The only
  way to get `false` is a well-formed `lock-before-sleep #false` in the
  file.

### Load-bearing KDL gotcha (read this before writing `session.kdl` fixtures anywhere downstream)

`Cargo.toml` pins `kdl = "6.7.1"` with **no `v1`/`v1-fallback` feature**
(matches both siblings exactly), so the parser is **KDL v2**. KDL v2
reserves bare `true`/`false`/`null` as ordinary identifiers — booleans
**must** be written `#true`/`#false`. A bare `lock-before-sleep false` is
not "wrong type for this knob", it makes the **whole document** fail to
parse (`KdlDocument::parse` returns `Err`, "Expected identifier string").
Verified against the `kdl` crate's own fixtures
(`kdl-6.7.1/src/document.rs`, e.g. `mouse_mode #false`) and empirically —
see `bareword_bool_typo_fails_whole_document_and_defaults_to_locking_on` in
`config.rs`'s test module, which proves the safe consequence: whole-document
parse failure still degrades to `SessionConfig::default()`, so
`lock_before_sleep` lands on `true` (safe) even for this specific typo, not
on the `false` the author meant to write. **Stage 8's README must show
`#true`/`#false` in its `session.kdl` reference, not bare
`true`/`false`** — this is the one place this schema deviates from what
would look natural coming from most other config languages (and from KDL
v1, which the siblings never had reason to hit either, since neither has a
bool knob yet).

### Resolution order

Identical three-rung chain to both siblings: `$SAOLA_CONFIG_DIR` →
`$XDG_CONFIG_HOME/saola` → `~/.config/saola`, file name `session.kdl`. Empty
env var == unset. No file / any I/O error → defaults silently. Malformed KDL
→ one `tracing::warn!` + defaults. Single bad knob → warn + that knob's
default, rest of document still loads.

### One deliberate deviation from copying the lockscreen's `config.rs` verbatim

`config_dir_from`'s nested `if let Some(x) = x { if !x.is_empty() { ... } }`
pattern, copied at first literally from the lockscreen, fails this repo's
`cargo clippy -D warnings` (`collapsible_if`, presumably a newer
clippy/edition-2024 lint than what the lockscreen's toolchain enforces).
Rewritten as `if let Some(x) = x && !x.is_empty() { ... }` (a stable
let-chain, edition 2024) — behavior identical, just collapsed per clippy's
own suggestion. Flagging this in case Stage 8 or a future audit diffs this
file against the lockscreen's and wonders why it isn't byte-for-byte the
same shape.

## `main.rs` — shutdown wiring (the seam Stage 4 fills)

- **`main()` stays synchronous.** It parses argv by hand (`Cli::parse`, no
  clap — two flags didn't earn a dependency) *before* any tokio runtime
  exists. `--version` and `--check-config` both return without ever
  constructing a runtime. Only the no-args path calls `run()`, which builds
  a `tokio::runtime::Builder::new_multi_thread().enable_all()` runtime by
  hand (still not `#[tokio::main]` — same reasoning as Stage 1's stub, now
  stated explicitly in `main`'s doc comment) and calls
  `runtime.block_on(async_main(config))`.
- **`--check-config`** calls the exact same `SessionConfig::load()` the real
  daemon path uses, then `println!("{config:#?}")` and exits 0 — so it can
  never drift from what the daemon would actually resolve. Verified by hand
  (see Evidence below): missing file → all defaults printed; a real file
  with `#false` and both idle timeouts → those exact values printed.
- **Shutdown fan-out**: `tokio::sync::watch::channel(false)` in
  `async_main`. Every module task gets its own `shutdown_rx.clone()`
  and does `let _ = shutdown_rx.changed().await;` to block until told to
  stop. `watch` was chosen over a `oneshot`-per-task specifically because a
  late-spawning receiver still observes the last-sent value — no
  missed-signal race.
- **Signal handling**: `wait_for_shutdown_signal()` installs
  `SignalKind::terminate()` and `SignalKind::interrupt()` via
  `tokio::signal::unix::signal` and `tokio::select!`s on both. If handler
  installation itself fails, it logs at error and returns immediately
  (falls through to shutdown) rather than looping or panicking.
- **Join-before-exit**: after `shutdown_tx.send(true)`, `async_main` drains
  a `tokio::task::JoinSet<()>` via `join_next().await` until empty, logging
  any `JoinError` (a module task panicking — should never happen per the
  no-panic rule, but `JoinSet` catches the unwind rather than taking the
  process down by itself, so we still log it loudly) before `async_main`
  returns and the process exits. **This is the exact point Stage 4's
  `sleep.rs` inherits**: when its module task is doing real work, "await it
  to completion after the shutdown signal" is what guarantees the logind
  delay-inhibitor fd it owns actually drops before the process exits,
  rather than racing OS-level process teardown.
- **Where Stage 4/5/6 plug in**: three stub async fns exist purely as
  placeholders and *are the seam*:
  - `sleep_module_stub(shutdown_rx)` → Stage 4 replaces this call
    (`tasks.spawn(sleep_module_stub(...))` in `async_main`) with
    `modules::sleep::run(..., shutdown_rx)` (or whatever Stage 4 names its
    entry point) — same "owns a `watch::Receiver`, returns when told to
    stop" shape.
  - `idle_module_stub(config.idle, shutdown_rx)` → Stage 5's replacement.
    Already takes `IdleConfig` by value, so Stage 5 only needs to change the
    function body, not the call site.
  - `inhibit_module_stub(shutdown_rx)` → Stage 6's replacement.
  - None of these stubs touch Wayland, D-Bus, or logind yet — `mod.rs`
    under `src/modules/` is still the Stage 1 doc-comment-only stub; no
    `sleep.rs`/`idle.rs`/`inhibit.rs` files exist. `cargo run` (no flags) at
    this stage is safe to run directly (no real session bus / Wayland /
    logind connection is opened) — confirmed by actually running it and
    sending SIGTERM (see Evidence).
- **Logging**: `init_tracing()` sets up `tracing_subscriber::fmt()` on
  stderr with `env-filter` (`RUST_LOG`, default `"info"`), called once at
  the top of `run()` — never in the `--version`/`--check-config` paths,
  which stay clean stdout-only for scriptability.

## Evidence

- `cargo build`: exit 0, clean.
- `cargo clippy --all-targets -- -D warnings`: exit 0, clean (after the
  `collapsible_if` fix noted above).
- `cargo test`: **24 passed, 0 failed** (`src/main.rs`'s unittests binary —
  `config::tests::*` is 20 of them, `tests::*` — the `Cli::parse` tests — is
  4). No `#[ignore]`d tests.
- `cargo fmt --check`: exit 0, clean.
- Manual CLI checks (all via `cargo run -q --`):
  - `--version` → `saola-session 0.1.0-dev`.
  - `--check-config` with no config file present → prints
    `SessionConfig { idle: IdleConfig { lock_after_minutes: None,
    power_off_after_minutes: None }, locker: "saola-lockscreen",
    lock_before_sleep: true }`.
  - `--check-config` against a real file (`idle { lock-after 5;
    power-off-after 10 } locker "swaylock" lock-before-sleep #false`) via
    `SAOLA_CONFIG_DIR` → printed values matched the file exactly, including
    `lock_before_sleep: false`.
  - `--bogus` → stderr usage message, exit code 1.
- Manual shutdown check: ran the built binary (`./target/debug/
  saola-session`, no flags) under `timeout --signal=TERM 2s`, `RUST_LOG=info`.
  Observed log sequence: config loaded → "starting" → all three module
  stubs log "not yet implemented" → (2s later) "received SIGTERM" →
  "shutdown signal received, stopping module tasks" → "stopped", all within
  ~1ms of the signal arriving. No Wayland/D-Bus/logind connection was
  opened (confirmed by the log content — nothing but the three stub
  messages), consistent with this being safe to run outside a nested-niri
  or real session at Stage 3.

## Nothing deferred

Everything the task listed shipped: `config.rs` parser + tests (including
explicit missing-file and nonsense-values coverage, plus the KDL v2
bareword-bool case above, which wasn't anticipated going in but turned out
to be exactly the "nonsense-values path" the task asked for), `main.rs`'s
event loop skeleton with signal-driven shutdown, and the `--version`/
`--check-config` CLI surface.
