# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0, still `0.1.0-dev` in `Cargo.toml`. Tagging `0.1.0` is Jordan's call,
made after the Jordan-run end-to-end suspend/resume sequence in `README.md`
passes against this code — see that section for the exact steps. From here
on, `release-plz` (`release-plz.toml`) manages version bumps and this file's
released sections from Conventional Commits.

### Fixed

Stage 8 fixed every must-fix finding from `docs/REVIEW-v0.1.md` (Stage 7's
exposure/silent-failure audit):

- **F-1** — an idle-lock suppressed by an active ScreenSaver inhibit now
  re-fires the moment the inhibit clears, instead of waiting for keyboard
  activity that might never come (the "walk away during a film, it ends,
  nobody comes back" exposure). `idle.rs`'s `Arm` gained a `Suppressed`
  state distinct from `Fired`, and `InhibitChanged(false)` re-evaluates
  every suppressed target.
- **F-2** — `contrib/systemd/saola-session.service` sets `KillMode=process`
  so a daemon restart no longer kills a lock screen the daemon had just
  spawned (systemd's default `control-group` kill mode would SIGKILL every
  process in the unit's cgroup on every stop, including the spawned
  locker).
- **E-1** — a logind connection failure at startup no longer latches the
  daemon into a permanently degraded state that still reports
  `active (running)`. `sleep::run` now retries the connection on a timer
  and keeps holding/re-acquiring the inhibitor once it succeeds.
  (`SwappableLogind` is the mechanism: the same `Logind` trait object
  shared between the sleep and idle modules can now be replaced in place
  once a reconnect succeeds.)
- **E-2** — a `systemd-logind` restart no longer leaves a stale, meaningless
  inhibitor file descriptor held forever. `sleep.rs` now subscribes to
  `org.freedesktop.DBus`'s `NameOwnerChanged` for `org.freedesktop.login1`
  and releases-then-reacquires the inhibitor when it fires.
- **E-3** — the idle module and the sleep module can no longer both spawn a
  locker inside the ~360ms window before `LockedHint` catches up to a
  spawn (measured in `docs/SIGNALS.md`). `SessionLocker` now shares a
  `last_spawn` timestamp across every clone of it, and a recent spawn is
  treated the same as an already-locked session for the purposes of
  spawning again (though the before-sleep path still waits out
  confirmation on it).
- **E-4** — a Wayland connect failure at daemon startup (a lost boot-order
  race against the compositor) no longer kills the process immediately.
  `idle.rs`'s Wayland thread now retries the initial connect with bounded
  backoff before giving up, and the shipped unit sets
  `RestartSec=2s`/`StartLimitIntervalSec=0` so systemd's own start-rate
  limiter can no longer turn a lost race into a permanently `failed` unit.
- **D-1** — the `org.freedesktop.ScreenSaver` shim now bounds both the
  length of the `app`/`reason` strings it stores and logs (256 characters,
  truncated on a UTF-8 character boundary) and the number of live grants
  (64 per peer, 1024 total), refusing further `Inhibit` calls past either
  cap without storing them (a cookie is still returned, so a capped caller
  degrades to "this inhibit has no effect" rather than getting a D-Bus
  error it likely handles worse).

### Also fixed (non-blocking findings, fixed anyway)

- **E-5** — folded into E-1's fix: the same retry timer that reconnects a
  degraded logind connection also retries acquiring the delay inhibitor
  when connected but not currently holding one.
- **E-6** — the before-sleep confirmation deadline default moved from 2s to
  4s (four-fifths of `InhibitDelayMaxUSec`, up from three-fifths), since
  the old ratio never actually bound on a typical 5s-cap machine — the
  flat default always won first, discarding real margin against a slow
  cold-boot locker start for no benefit against logind's suspend-blocking
  cap.
- **L-1** — `--check-config` now installs a tracing subscriber
  (defaulting to `warn`, unlike the real daemon's `info`) before loading
  the config, so the per-knob warnings the loader emits are no longer
  silently discarded.
- **L-2** — a signal-handler installation failure at startup
  (`SIGTERM`/`SIGINT`) now exits non-zero instead of zero, so
  `Restart=on-failure` actually restarts a daemon that can't hear shutdown
  signals, rather than leaving it stopped for good.
- **L-3** — corrected a comment and a journal line in `inhibit.rs` that
  claimed the peer-vanish watcher's death was scoped to one task and left
  `Inhibit`/`UnInhibit` still working; it is actually awaited as the tail
  of the module's whole task and its death is fatal to the process (which
  is the *right* behavior — only the words describing it were wrong).
- **L-4** — an `Inhibit` call arriving with no D-Bus `sender` header (never
  observed, believed structurally unreachable per Stage 6/7's review) is
  now refused rather than stored under an empty peer name, which would
  have been an ungrace-cleanable permanent grant if it ever happened.
- **L-6** — `async_main`'s top-level `select!` now has `biased;`, matching
  every module's own event loop, so a clean shutdown always wins a
  simultaneous "a module task also just ended" race instead of a coin
  flip occasionally logging a false-positive `ERROR` and exiting 1 on an
  ordinary logout.

### Won't fix (documented, not blocking)

- **L-5** — with logind degraded (nested-niri testing only — never a real
  session), idle-lock spawns a fresh locker on every idle cycle instead of
  detecting an already-locked session, because there is no `LockedHint` to
  read in that environment. Severity 3 (a stray spurious locker, never
  exposure), bounded to an environment that is by definition not a real
  session, and now documented in `README.md`'s known-limitations section.
  Not worth the complexity of a liveness-tracking fallback for an
  environment this daemon's own design already treats as best-effort.
- **I-2** — `InhibitStore`'s cookie counter uses `wrapping_add` rather than
  a checked increment. Confirmed out of scope by Stage 7's review: 2³²
  concurrent grants is an out-of-memory event well before it is a wrap
  (and D-1's new `MAX_GRANTS_TOTAL` cap makes that path impossible
  outright), 2³² sequential grant/release pairs is days of sustained
  D-Bus traffic, and even a wrap's outcome is safe by direction — a
  collided cookie makes an honest app's inhibit silently stop working
  (idle actions resume), which is severity 3, never severity 1.

### Added

- `contrib/systemd/saola-session.service` — the user-level `systemd --user`
  unit, packaging the F-2/E-1/E-4 fixes above into its `Restart=`,
  `RestartSec=`, `StartLimitIntervalSec=` and `KillMode=` settings.
- `README.md` — what this daemon is, the severity order, build/install, the
  full `session.kdl` reference, `--check-config`, known limitations, the
  exact commands to retire `saola-lockscreen`'s interim `contrib/session/`
  wiring, a PR-ready note for that sibling repo, and the Jordan-run
  end-to-end test sequence that gates tagging `0.1.0`.
- `contrib/aur/PKGBUILD` — the zero-touch packaged install (mirrors
  `saola-panel`'s CI-substituted template): binaries in `/usr/bin`, the
  unit in `/usr/lib/systemd/user/`, and a static
  `graphical-session.target.wants/` enablement symlink so end users never
  run `systemctl --user enable` — install, log in, `loginctl lock-session`
  works. The unit gained a matching
  `ConditionEnvironment=XDG_CURRENT_DESKTOP=niri` gate so that symlink
  can't start the daemon under a different DE on the same machine, where
  its inhibitor/ScreenSaver-shim/locker would fight the resident session
  infrastructure. `README.md` gained the corresponding "Zero-touch
  install" section, plus a troubleshooting note (observed live 2026-08-03)
  for the dev-install-only failure where the user manager's `PATH` lacks
  `~/.local/bin` and the locker spawn fails with `No such file or
  directory`.
- 17 new unit tests covering every must-fix finding's fixed behavior (2
  cross-clone concurrency tests for E-3, 3 for E-2's `LogindRestarted`
  event, 5 for F-1's re-fire-on-clear behavior including one at the
  `IdleExecutor` level, 7 for D-1's bounds and UTF-8-safe truncation) plus
  2 pre-existing deadline tests updated for E-6's ratio/default change
  (one's expected value, `600ms → 800ms`; one's name and comment, since its
  assertion already held at the new default). One pre-existing test
  (`a_full_sleep_resume_cycle_can_sleep_again`) needed no assertion change
  but did drive a design correction while fixing E-3 — see the F-1/E-3
  section above's `SessionLocker::lock_if_needed` note: `LockedHint` is
  now always checked before the settle-grace timestamp, not after, so an
  already-locked session (confirmed by the real signal) is never
  overridden by the grace heuristic. Test count: 76 (Stage 7's baseline)
  → 93.
