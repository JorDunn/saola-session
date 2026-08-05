# Stage 7 handoff — exposure and silent-failure audit (read-only)

Compressed state for Stage 8 (fixes, packaging, docs, release prep).
**Read `docs/REVIEW-v0.1.md` first** — it has the full reasoning, failure
scenarios and fix sketches. This file is the actionable index plus the
things Stage 8's *packaging* work specifically needs.

## Gate status as of this stage (nothing was changed)

| Command | Exit | Detail |
|---|---|---|
| `cargo build` | 0 | — |
| `cargo clippy --all-targets -- -D warnings` | 0 | no diagnostics; run after `cargo clean -p saola-session`, so a real compile |
| `cargo test` | 0 | **76 passed**, 0 failed, 0 ignored (config 23, sleep 25, inhibit 14, idle 10, main 4) |
| `cargo fmt --check` | 0 | — |

`git status --porcelain` is byte-identical to the start of the stage. The
only new paths are `docs/REVIEW-v0.1.md` and this file. **No source edits
were made** — every finding is Stage 8's to fix.

## Findings index

Severity is against Architecture's order (1 = exposed, 2 = suspend
blocked/locked out, 3 = spurious lock), not CVSS.

| ID | Sev | Verdict | Where | Fix direction (one line) |
|---|---|---|---|---|
| **F-1** | High | **must-fix** | `idle.rs:240–247`, `:219–222` | Split `Arm::Suppressed` from `Arm::Fired`; on `InhibitChanged(false)` re-fire suppressed targets instead of waiting for a `Resumed` that may never come |
| **F-2** | High | **must-fix** | unit file + `sleep.rs:742–782` | `KillMode=process` in the unit so a daemon restart doesn't SIGKILL the lock screen it spawned (systemd's default `control-group` kills the whole cgroup on stop) |
| **E-1** | High | **must-fix** | `sleep.rs:949–986`, `:1149–1153` | Degraded logind mode is latched forever and reports `active (running)` — retry `connect_logind` on a 30 s timer and keep warning while degraded |
| **E-2** | High | **must-fix** | `sleep.rs:605–609` | A `systemd-logind` restart leaves a stale inhibitor fd that `ensure_inhibitor`'s `is_some()` short-circuit never replaces; subscribe to `NameOwnerChanged` arg0=`org.freedesktop.login1`, then release-then-re-acquire |
| **E-3** | Med | **must-fix** | `sleep.rs:285–289`, `:309–346` | idle and sleep tasks both spawn inside the ~360 ms `LockedHint` lag; add shared `last_spawn: Arc<Mutex<Option<Instant>>>` + a 2 s grace treated as already-locked |
| **E-4** | Med | **must-fix** | `idle.rs:649–667` + unit file | Wayland connect failure at startup is fatal → systemd's default `StartLimitBurst=5`/`10s` makes the unit *permanently* failed; `RestartSec=2s` + `StartLimitIntervalSec=0`, and retry the connect in `wayland_thread_main` |
| **E-5** | Med | nice-to-have | `sleep.rs:605–644` | No background retry after 3 failed `Inhibit()` attempts (750 ms budget); fold into E-1's timer |
| **E-6** | Med | nice-to-have | `sleep.rs:113`, `:706–715` | Flat 2 s deadline always wins over the 3/5 rule and discards 3 s of this machine's 5 s budget; raise to 4 s / four-fifths |
| **D-1** | Med | **must-fix before the shim can ever go Active** | `inhibit.rs:174–192`, `:450–481` | No bound on `app`/`reason` length or grant count — a session-bus peer can OOM the lock daemon; cap 256 chars (char-boundary-safe), 64/peer, 1024 total, refuse past cap without storing |
| **L-1** | Low | nice-to-have (do it) | `main.rs:83–91` | `--check-config` never calls `init_tracing()`, so every per-knob warning is dropped — the README is about to point operators at this flag |
| **L-2** | Low | nice-to-have | `main.rs:257–279`, `:214–218` | Signal-handler install failure exits **0**, so `Restart=on-failure` won't restart |
| **L-3** | Low | nice-to-have | `inhibit.rs:595–615`, `:633–640` | Comment and journal line both claim the daemon keeps running when the `NameOwnerChanged` stream ends; it exits within ms. Keep the fatality, fix the words |
| **L-4** | Low | nice-to-have | `inhibit.rs:464` | Empty `sender` header → untrackable permanent grant; refuse to store rather than `unwrap_or_default()` (Stage 6 asked for this ruling: "believed unreachable" is right but not worth relying on) |
| **L-5** | Low | **won't-fix — document** | `sleep.rs:927–935`, `:310–322` | With logind degraded, idle-lock stacks a locker every cycle (nested-niri only). README note in the nested-testing section is sufficient |
| **L-6** | Low | nice-to-have | `main.rs:214–226` | No `biased;` in `async_main`'s `select!` (all three modules have one) → exit 1 + a false-positive `ERROR` at every logout |
| **I-2** | Info | **won't-fix — settled** | `inhibit.rs:182` | Cookie `wrapping_add`: Stage 6 asked for a ruling. Out of scope **confirmed** — 2³² concurrent grants is D-1's OOM first, 2³² sequential is days of round trips, and the outcome is safe by direction (an inhibit is lost → idle resumes → severity 3). Fixing D-1 closes it anyway |

## What Stage 8's packaging work must know

**Task-death semantics vs `Restart=on-failure` — they match, with three
exceptions that are findings.** `main.rs:214–226` `select!`s shutdown against
`tasks.join_next()`; **any** task returning or panicking before shutdown yields
`ExitCode::FAILURE`. `JoinSet` catches a task panic rather than aborting, but
the `JoinError` still counts as a completion, so a panic also exits non-zero.
So `Restart=on-failure` is the correct policy. The exceptions:

- **L-2** — one path exits **0** while broken (won't restart).
- **E-1** — one path never returns at all while broken (won't restart, and
  reports `active (running)`). This is the one that most needs fixing before
  the unit ships, because it defeats the restart policy entirely.
- **L-6** — a clean logout can exit 1, producing a spurious `ERROR` in the
  very journal the README will tell Jordan to trust.

**Unit-file requirements this audit produced (all of them are findings, not
preferences):**

```ini
[Service]
Restart=on-failure
RestartSec=2s              # E-4: not the 100ms default
KillMode=process           # F-2: do NOT kill the spawned locker on restart
[Unit]
StartLimitIntervalSec=0    # E-4: never give up on a severity-1 daemon
After=graphical-session.target
After=niri.service         # E-4: order against what provides the socket
PartOf=graphical-session.target
```

Install instructions stay user-level (`install -Dm644` +
`systemctl --user enable --now`) — no root anywhere, per the sudo rule.

**README must say (each backed by a finding):**

- `active (running)` proves nothing — the real check is
  `systemd-inhibit --list | grep saola-session` (E-1).
- `WAYLAND_DISPLAY` must be in the user manager's environment or the locker
  spawn silently fails to lock; symptom is the `Unconfirmed` warning only
  (I-6). Verify `loginctl lock-session` **under systemd**, not just under
  `cargo run` — `cargo run` inherits a full environment and cannot reproduce
  this.
- Known limitation: in a nested niri (no logind) idle-lock stacks lockers
  (L-5) — expected, not a bug.
- Known limitation: lock confirmation is `LockedHint` polling, per
  `docs/SIGNALS.md`; a deadline-expiry release lets the suspend proceed with
  the lock unconfirmed (E-6 raises the deadline but does not remove the case).
- Advisory tooling (`cargo audit` / `cargo deny`) is **not installed** on this
  machine and was not run for this review — consider wiring one into CI (I-3).

**End-to-end sequence:** two findings can only be falsified by Jordan's run,
so call them out in the README's expected observations — **E-2**'s stale-fd
behaviour (visible as a suspend that does not wait) and **I-6**'s environment
requirement. Run the sequence against the *fixed* code.

**CHANGELOG:** document the two deliberate won't-fixes with reasoning —
**L-5** (degraded-mode locker stacking, nested-only, severity 3) and **I-2**
(cookie wrapping, safe by direction).

## Verified clean — do NOT re-audit these

Full evidence is in `docs/REVIEW-v0.1.md`'s "Checked and found clean" section.
Summary so Stage 8 doesn't spend context re-deriving it:

- **Panic surface: zero hits.** Per-file scan above each `#[cfg(test)]`
  boundary for `unwrap()`/`expect(`/`panic!`/`unreachable!`/`todo!`/
  `unimplemented!`/`assert` → **0 code hits across all six files**. No
  `unsafe` anywhere. One arithmetic site (`config.rs:351`), provably
  non-overflowing. `checked_mul` used correctly in both places it is needed.
  No `[profile]` section, so `panic = "unwind"`, which is what makes the
  JoinSet and Wayland-thread death plumbing work.
- **Stage 6's three open questions, all resolved:** (1) `lock_store`'s
  poisoned-mutex recovery is sound — there is genuinely no panic path inside
  any guarded section, keep it. (2) The `inhibit_closed` latch cannot
  mis-latch — `main.rs` constructs exactly one such channel and *moves*
  `inhibit_tx`; no stub exists. (3) `Shared::apply` is the only `send` call
  site, and the transition is computed inside `InhibitStore`, so
  "send on zero↔nonzero only" is correct by construction.
- **"Inhibits gate idle only, never sleep" holds structurally** — re-verified
  by grep, not taken on faith: `inhibit.rs`'s *only* reference to `sleep` is
  `use crate::modules::sleep::BoxFuture` (a type alias) at `:75`. If Stage 8
  touches `inhibit.rs`, re-run that grep before finishing.
- **Sleep state machine transitions** match Architecture's diagram edge for
  edge; every `SleepReadiness` path releases; resume re-acquires *first*
  (asserted by a journal-ordering test, not a comment); `Unlock` is
  deliberately inert and must stay that way (acting on it would make
  `loginctl unlock-session` an auth bypass).
- **`connect`'s subscribe-then-inhibit ordering** is correct — do not
  "optimise" it to inhibit-first.
- **`CacheProperties::No`** on both logind proxies is load-bearing (a stale
  `LockedHint` is the one belief that must never be wrong) — do not re-enable
  caching for performance.
- **`resolve_session_path`**'s three rungs are correct, including avoiding
  `/session/auto` (which never emits signals) and the two `…ByPID` name
  overrides.
- **Spawn hygiene** clean on all three of Architecture's requirements: cannot
  block the loop, no output parsing, no zombies (`drop(child)` + `enable_all()`),
  no shell (`split_command` is plain `argv`). The only spawn gaps are E-3
  (cross-task double spawn) and F-2 (cgroup membership).
- **The name claim** uses `DoNotQueue` alone — never add `ReplaceExisting`
  (it would silently steal the name from niri and break niri's own idle
  suppression for every app on the machine).
- **Config resilience**: no bad-config path can resolve `lock_before_sleep`
  to `false`, verified on all four branches.
- **Shutdown ordering** is genuinely ordered — the inhibitor fd closes before
  the process exits; there is no `std::process::exit` in the crate.
- **Dependencies**: 81 unique crates, no C bindings beyond `libc`, no
  openssl/native-tls, 3 duplicate pairs all build-time only, nothing
  unmaintained. Nothing to change in `Cargo.toml`.

## Things this review could not check

- No suspend/resume has ever run against this daemon (agents may not suspend).
- The inhibitor's *release-then-suspend-proceeds* edge has never run live;
  only acquire-and-hold has.
- The ScreenSaver shim's `Active` path has only run under a private
  `dbus-run-session` bus, never on a real session bus (niri owns the name).
- niri's behaviour when an `ext-session-lock-v1` client dies while holding
  the lock is taken from the protocol and from `saola-lockscreen`'s
  `CLAUDE.md`, not observed. It changes F-2's *consequence* (severity 2 vs
  severity 1) but not F-2's fix.
