# Stage 8 handoff — fixes, packaging, docs, release prep

Compressed state for whoever reads this next (Jordan, or a re-run of this
stage). Full reasoning for every fix lives in the source comments named
below (each fix names its finding ID inline) and in `CHANGELOG.md`'s
`[Unreleased]` section. This file is the index.

## Gate status (after all fixes — run yourself to re-verify)

| Command | Exit |
|---|---|
| `cargo build` | 0 |
| `cargo clippy --all-targets -- -D warnings` | 0 |
| `cargo test` | 0 — **93 passed**, 0 failed, 0 ignored (76 at Stage 7's baseline + 17 new) |
| `cargo fmt --check` | 0 |

Test breakdown of the 17 new: `sleep.rs` +5 (2 for E-3's cross-clone
concurrency, 3 for E-2's `LogindRestarted`), `idle.rs` +5 (4 pure-machine
tests for F-1's re-fire-on-clear, 1 `IdleExecutor`-level test that
`Multiple` actually executes each sub-outcome), `inhibit.rs` +7 (3 for
`truncated`'s char-boundary safety, 4 for D-1's per-peer/total caps).
2 pre-existing `sleep.rs` deadline tests were updated for E-6 (one value,
`600ms → 800ms`; one name/comment only, since its assertion already held at
the new default).

## Findings fixed vs deferred

All 7 must-fix findings from `docs/REVIEW-v0.1.md` are fixed. All
non-blocking findings were also fixed except two, which are documented
won't-fixes (both already ruled on by Stage 7's review, restated here with
the reasoning).

| ID | Verdict | Fixed how | Where |
|---|---|---|---|
| F-1 | **must-fix — fixed** | `Arm::Suppressed` split from `Arm::Fired`; `InhibitChanged(false)` walks both targets and re-fires anything still `Suppressed`, returning `IdleOutcome::Multiple` for `IdleExecutor::execute` to run in sequence | `src/modules/idle.rs` |
| F-2 | **must-fix — fixed** | `KillMode=process` in the unit, with a comment naming the finding | `contrib/systemd/saola-session.service` |
| E-1 | **must-fix — fixed** | `run`'s degraded branch is now a retry loop (`RECONNECT_INTERVAL` = 30s) instead of a permanent park; `SwappableLogind` lets a successful reconnect update the *same* `Logind` trait object `SessionLocker` (shared with `idle.rs`) and `SleepMachine` both hold | `src/modules/sleep.rs` |
| E-2 | **must-fix — fixed** | Subscribed to `org.freedesktop.DBus`'s `NameOwnerChanged` filtered to `arg0='org.freedesktop.login1'`; a new `SleepEvent::LogindRestarted` releases-then-reacquires the inhibitor | `src/modules/sleep.rs` |
| E-3 | **must-fix — fixed** | `SessionLocker` gained a shared `last_spawn: Arc<Mutex<Option<Instant>>>`; `lock_if_needed` checks `LockedHint` first (still authoritative when it says `true`), then — only if `LockedHint` says `false` — checks whether a spawn happened within `LOCK_SETTLE_GRACE` and treats that as `LockAttempt::SpawnPending` (skip spawn, but the before-sleep path still waits out confirmation on it) | `src/modules/sleep.rs` |
| E-4 | **must-fix — fixed** | `wayland_thread_main` retries the initial connect up to `STARTUP_CONNECT_ATTEMPTS` (10) with `STARTUP_CONNECT_BACKOFF` (2s) between attempts before giving up; unit sets `RestartSec=2s`, `StartLimitIntervalSec=0` | `src/modules/idle.rs`, `contrib/systemd/saola-session.service` |
| D-1 | **must-fix — fixed** | `MAX_FIELD_LEN` (256, char-boundary-safe truncation via `truncated()`), `MAX_GRANTS_PER_PEER` (64), `MAX_GRANTS_TOTAL` (1024); a capped `Inhibit` still returns a cookie but stores nothing (`Admission::RefusedPerPeerCap`/`RefusedTotalCap`) | `src/modules/inhibit.rs` |
| E-5 | nice-to-have — fixed | Folded into E-1's timer: the same retry loop also retries `ensure_inhibitor` when connected but not holding one (`select!`'s `if lock_before_sleep && !machine.holds_inhibitor()` guard) | `src/modules/sleep.rs` |
| E-6 | nice-to-have — fixed | `DEFAULT_CONFIRMATION_DEADLINE` 2s → 4s; ratio 3/5 → 4/5 of `InhibitDelayMaxUSec` | `src/modules/sleep.rs` |
| L-1 | nice-to-have — fixed | `--check-config` now calls `init_tracing("warn")` before loading the config | `src/main.rs` |
| L-2 | nice-to-have — fixed | `wait_for_shutdown_signal` returns `bool`; a handler-install failure now resolves to `ExitCode::FAILURE`, not `SUCCESS` | `src/main.rs` |
| L-3 | nice-to-have — fixed | Corrected the `None`-branch comment and `error!` text in `watch_for_vanished_peers`; corrected `run`'s doc comment to name this as the one fatal path | `src/modules/inhibit.rs` |
| L-4 | nice-to-have — fixed | Empty `sender` header now refused (`warn!`, return cookie `0`, nothing stored) instead of `unwrap_or_default()` | `src/modules/inhibit.rs` |
| L-6 | nice-to-have — fixed | `biased;` added to `async_main`'s top-level `select!` | `src/main.rs` |
| L-5 | **won't-fix — documented** | Nested-niri-only (no real session ever hits it), severity 3, bounded to an environment this daemon already treats as best-effort. Documented in `README.md`'s known-limitations section and `CHANGELOG.md` | — |
| I-2 | **won't-fix — documented** | Already ruled out of scope by Stage 7 (wrapping cookie counter is safe by direction, and D-1's new `MAX_GRANTS_TOTAL` cap forecloses the concurrent-wrap path entirely). Restated with reasoning in `CHANGELOG.md` | — |

Info findings (I-1, I-3 through I-7) needed no code change — Stage 7 marked
them either settled or advisory-only (e.g. I-3's `cargo audit`/`cargo deny`
recommendation, which is a CI wiring task, not something this stage's
`Cargo.toml` needed to change).

## A design note worth knowing about before touching `sleep.rs` again

Fixing E-1 required more structural change than the review's fix sketch
spelled out in full: retrying `connect_logind()` on its own only fixes
`SleepMachine`'s own copy of the `Logind` trait object. `SessionLocker` (the
single spawn-if-not-locked path, cloned into `idle.rs`'s task at startup)
holds its *own* `Arc<dyn Logind>`, taken from the same `connect()` call — a
naive retry that just builds a new `SleepMachine` around a fresh connection
would leave `SessionLocker`'s `LockedHint` checks (and idle.rs's clone of
it) permanently pointed at the connection that failed at startup.

The fix is `SwappableLogind` (`sleep.rs`): a `Logind` implementation that
forwards through a `tokio::sync::RwLock<Arc<dyn Logind>>`, constructed once
by `connect()` and shared — via the *same* `Arc<dyn Logind>` — between
`SessionLocker` and `SleepMachine`. `SwappableLogind::replace` (called from
`run`'s retry loop on a successful reconnect) updates both at once. This
also had to interact correctly with E-3's `last_spawn` sharing: the fix
does *not* rebuild `SessionLocker` on reconnect (which would have handed
`idle.rs` a stale clone with a disconnected `last_spawn`), only its
`logind` field's *target* changes underneath it.

Also worth knowing: `SessionLocker::lock_if_needed`'s check order matters.
It must read `LockedHint` **before** consulting the `last_spawn` grace
window, not after — an already-locked session (an authoritative `true` from
the real signal) must never be second-guessed by the settle-grace
heuristic, which exists only to cover the case where `LockedHint` still
reads `false` because it hasn't caught up yet. Getting this order backwards
broke `a_full_sleep_resume_cycle_can_sleep_again` during development (it
returned `Confirmed` instead of the expected `AlreadyLocked` on the second
sleep cycle) — that regression is what surfaced the ordering requirement,
and the fixed order is what's in the tree now.

## Packaging: `contrib/systemd/saola-session.service`

Every non-default setting traces to a Stage 7 finding, named inline in the
unit file's own comments:

```ini
[Unit]
After=graphical-session.target niri.service   # E-4
PartOf=graphical-session.target
StartLimitIntervalSec=0                        # E-4

[Service]
ExecStart=/usr/bin/saola-session
Restart=on-failure
RestartSec=2s                                  # E-4
KillMode=process                               # F-2

[Install]
WantedBy=graphical-session.target
```

Install is user-level throughout (`install -Dm755`/`-Dm644` +
`systemctl --user enable --now`), matching the sudo rule — see `README.md`'s
Install section for the exact commands.

## Exact commands Jordan runs

**Retiring `saola-lockscreen`'s interim wiring** (README.md has the full
section with reasoning; commands only, here):

```bash
systemctl --user disable --now saola-lock-before-sleep.service
rm -f ~/.local/bin/saola-lock-before-sleep
rm -f ~/.config/systemd/user/saola-lock-before-sleep.service
systemctl --user daemon-reload
# If ever added, remove this line from ~/.config/niri/config.kdl:
#   spawn-at-startup "swayidle" "-w" "timeout" "300" "saola-lockscreen"
systemctl --user list-units 'saola-lock-before-sleep*'   # verify: empty
```

A PR-ready retirement note for `saola-lockscreen/contrib/session/README.md`
is written out verbatim in this repo's own `README.md` (its own section,
"PR-ready note for...") — **not applied to the sibling repo**, per this
stage's constraint against editing anything outside this repo. Jordan (or a
future stage in that repo) copies it over.

**Enabling this daemon:**

```bash
cargo build --release
install -Dm755 target/release/saola-session ~/.local/bin/saola-session
install -Dm644 contrib/systemd/saola-session.service ~/.config/systemd/user/saola-session.service
# Edit ExecStart= in the copied unit if the binary isn't at /usr/bin/saola-session.
systemctl --user daemon-reload
systemctl --user enable --now saola-session.service
```

## The end-to-end sequence (README.md has the full version with expected output)

1. `systemctl --user status saola-session.service` → `active (running)`
   (necessary, not sufficient — see step 2).
2. `systemd-inhibit --list | grep saola-session` → one `sleep`/`delay` row.
   This is the real "is it working" check; `active (running)` alone proves
   nothing (E-1's finding, now mitigated but not eliminated — a daemon that
   is *still* retrying a reconnect 30s in will show `active` with nothing
   in this output yet).
3. `loginctl lock-session` → screen locks within ~360ms (warm,
   `docs/SIGNALS.md`'s measurement). Must be checked with the daemon
   running **under systemd**, not `cargo run` — I-6's `WAYLAND_DISPLAY`
   environment gap only reproduces there.
4. A short `idle.lock-after "20s"` in `session.kdl`, restart the unit,
   stop touching the keyboard — locker spawns at ~20s, and again after a
   second idle period (proves re-arm).
5. **Only after 1–4 pass**: `systemctl suspend` (or a real lid close),
   twice, confirming the locker is on screen at the *moment* the display
   comes back, never the desktop. Check `journalctl --user -u saola-session`
   for `lock confirmed` vs. `lock NOT confirmed within the deadline` (the
   second is still safe, per the design, but worth knowing which happened).
   This step falsifies two things nothing upstream of it can: E-2's
   stale-fd-after-a-logind-restart behavior (only observable across an
   actual suspend/resume where logind itself might have restarted
   in between) and I-6's environment requirement under real suspend timing.

**Tagging `0.1.0` is Jordan's call, made only after step 5 succeeds at least
twice.** `Cargo.toml` stays `0.1.0-dev` until then. `release-plz.toml` is in
place (mirrors `saola-panel`'s, same `git_tag_name` pattern
`{{ package }}-v{{ version }}`) so tagging itself, once decided, is a normal
release-plz-driven bump from Conventional Commits going forward.

## Files this stage touched

Fixes: `src/modules/sleep.rs`, `src/modules/idle.rs`, `src/modules/inhibit.rs`,
`src/main.rs`.

New: `contrib/systemd/saola-session.service`, `CHANGELOG.md`,
`release-plz.toml`, this handoff.

Rewritten: `README.md` (was a two-line placeholder).

Untouched: `src/config.rs` (no findings against it), `docs/SIGNALS.md`,
`PLAN.md`, `CLAUDE.md`, all earlier handoffs, `docs/REVIEW-v0.1.md` itself
(Stage 7's review — read-only input to this stage, not edited).

Never touched, per the binding constraints: no `sudo`, no `systemctl
suspend`, no real lock/suspend of the actual session, the daemon was never
run against the real Wayland session, nothing outside this repo was edited
(the sibling-repo PR note is text inside this repo's own `README.md`, not a
commit to `saola-lockscreen`), no commit was made, no tag was made, version
stayed `0.1.0-dev`.
