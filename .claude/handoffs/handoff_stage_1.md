# Stage 1 handoff — crate skeleton + dependency resolution

## State

`cargo build` exits 0 at repo root. `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` are also clean (checked as a bonus —
Stage 1's verify contract only requires `cargo build`, but Stage 3's contract adds
both, so it's worth knowing they already pass on the stub). `cargo run` prints
`saola-session 0.1.0-dev`.

## File layout as created

```
Cargo.toml            # package + deps, survey comments inline (see below)
Cargo.lock             # committed by `cargo build`; matches the resolved versions below
rust-toolchain.toml    # `channel = "stable"`, copied verbatim from the lockscreen
rustfmt.toml           # single comment line, defaults, copied verbatim
LICENSE-MIT             # dual license pair, copied verbatim from the lockscreen
LICENSE-APACHE           # (byte-identical diff-checked; same copyright holder/year)
CLAUDE.md               # this repo's own, derived from the lockscreen's
src/main.rs             # stub: prints version, exits. NOT #[tokio::main] yet —
                         # no async work exists to justify a runtime; Stage 3 adds
                         # the attribute when main.rs gets real module tasks
src/config.rs           # doc-comment-only stub stating config.rs's role + the
                         # session.kdl schema sketch from Architecture; no parse
                         # code yet (Stage 3)
src/modules/mod.rs       # doc-comment-only stub; no sleep.rs/idle.rs/inhibit.rs yet
```

The single pre-existing `LICENSE` (MIT-only) was removed; its text is byte-identical
to the new `LICENSE-MIT`, so nothing was lost. `.gitignore`/`README.md` were kept
untouched (README still just has the one-line description — Stage 8 fills it in).
`cargo init` appended a redundant `/target` block to `.gitignore` — harmless, and the
lockscreen's `.gitignore` has the exact same cargo-init appendix, so it's not a
deviation to fix.

## Dependencies resolved (exact versions, `cargo tree -e normal`, 2026-08-02)

| Crate | Version | Cargo.toml features |
|---|---|---|
| `tokio` | 1.53.1 | `default-features = false`, `["rt-multi-thread", "macros", "signal", "time", "sync", "process"]` |
| `zbus` | 5.18.0 | `default-features = false`, `["tokio"]` |
| `wayland-client` | 0.31.15 | default |
| `wayland-protocols` | 0.32.13 | `["staging", "client"]` |
| `kdl` | 6.7.1 | default (matches both siblings' pin exactly) |
| `tracing` | 0.1.44 | default |
| `tracing-subscriber` | 0.3.23 | `["env-filter"]` |

Full reasoning for each is in `Cargo.toml`'s inline comments (verbose, in this repo's
established teaching-note style) and condensed in `CLAUDE.md`'s Conventions section.
The one-line versions for quick reference:

- **D-Bus crate: `zbus`, not `dbus-rs`.** `dbus-rs` binds the system `libdbus` C
  library (pkg-config/headers needed at build time, sync API with async bolted on
  separately); `zbus` is pure Rust, native tokio-integrated async, and already what
  `saola-panel` uses (`default-features = false, features = ["tokio"]` — identical
  line). Matching it means every Saola component shares one D-Bus wire
  implementation on the session bus, not two.
- **Logging: `tracing` + `tracing-subscriber`, not `eprintln!`.** Both siblings use
  bare `eprintln!` — fine for an interactive iced app with someone watching the
  screen. This is a headless daemon; Architecture's severity order (exposure >
  blocked-suspend > spurious-lock) needs to survive into `journalctl` as filterable
  levels. `tracing-subscriber`'s `fmt` layer writes to stderr, which systemd already
  journals per-unit — deliberately did *not* add `tracing-journald` (exists,
  maintained, but couples the binary to systemd's journal socket directly for no
  benefit `journalctl`-reading-stderr doesn't already give). `env-filter` feature
  enables `RUST_LOG` verbosity control without a recompile.
- **`ext-idle-notify-v1` feature flag, verified not assumed**: `wayland-protocols`
  needs `features = ["staging", "client"]` — it's a *staging* protocol, not in the
  stable set the default features expose. Verified with a throwaway crate (not just
  read off docs.rs): with those two features, the types compile and resolve at
  `wayland_protocols::ext::idle_notify::v1::client::{ext_idle_notifier_v1::
  ExtIdleNotifierV1, ext_idle_notification_v1::ExtIdleNotificationV1}`. Stage 5 needs
  exactly this path for its `ExtIdleNotifierV1`/`ExtIdleNotificationV1` bindings.
- **`tokio` features are wider than the siblings' `rt`+`sync`, deliberately.** The
  siblings get a runtime for free from iced's own `tokio` feature and only ever
  `spawn_blocking` onto it. This daemon *is* the runtime owner (Architecture:
  "main.rs: tokio event loop") and runs several independent long-lived tasks
  concurrently (sleep's logind listener, idle's Wayland dispatch, inhibit's D-Bus
  service), so `rt-multi-thread` was chosen over `current_thread`. `signal` is for
  Stage 3's SIGTERM/SIGINT shutdown; `time` for `LockPending`'s hard deadline and
  idle re-arm timers; `sync` for the sync-Wayland-to-async bridge channel Stage 5
  will need (the panel's "thread bridge" pattern, used there for libpulse/udev) plus
  shutdown fan-out; `process` for `tokio::process` (locker spawn, `niri msg action
  power-off-monitors`). `net` was deliberately *not* added directly — zbus's own
  `tokio` feature pulls whatever socket I/O it needs, and Cargo unifies dependency
  features across the graph, so a direct `net` feature here would be a no-op, not a
  size reduction.

## Surprises / gotchas for the next stage

1. **`edition = "2024"`, deliberately ahead of the siblings.** Both siblings are
   on 2021, but Jordan decided (2026-08-02, after Stage 1 verification) to pin this
   crate to 2024 and migrate the siblings later. Do NOT "fix" this back to 2021 for
   consistency — the inconsistency is intentional and temporary.
2. **`version = "0.1.0-dev"`**, matching the lockscreen's pre-release convention
   (not the panel's, which is post-first-release at `0.2.1`). Stays `0.1.0-dev`
   until Stage 8/Jordan's end-to-end suspend test, per PLAN.md's Stage 8 section.
3. **No `niri-ipc` crate dependency added.** Architecture's idle module talks to
   niri only via `tokio::process` + `niri msg action power-off-monitors` (a shell
   command, not the IPC socket) for Stage 1's dependency list. If Stage 2's signal
   research finds niri's IPC event stream (`niri msg event-stream`) is the better
   lock-state signal, that stage's handoff should flag whether a `niri-ipc` crate
   dependency (the panel already has one, pinned `=26.4.0`, typed structs vendored
   from niri's own source) is worth adding then — it wasn't needed for Stage 1.
4. **`src/main.rs` is intentionally not `#[tokio::main]` yet.** A version-print stub
   has no async work; adding the attribute now would stand up an unused
   multi-thread runtime for nothing. Stage 3 adds it when main.rs gains real
   module-task spawning.
5. `LICENSE-MIT`'s text was verified byte-identical to the lockscreen's via `diff`
   before deleting the old single `LICENSE` — same copyright holder ("Jordan Dunn")
   and year (2026), so no wording drifted.
6. Both `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` already
   pass cleanly on the Stage 1 stub, even though Stage 1's own verify contract
   doesn't require either — one less thing for Stage 3 to worry about inheriting.

## What Stage 2 needs from here

Nothing dependency-wise — Stage 2 is read-only research (`docs/SIGNALS.md`) and
doesn't touch `Cargo.toml`. It can `cargo build`/`cargo run` this stub as-is to
confirm the environment, but its real work is niri source reading and Jordan-driven
`busctl`/`loginctl`/`niri msg` probes, none of which need new dependencies.
