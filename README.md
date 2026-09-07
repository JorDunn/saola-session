# saola-session

Idle, sleep and lock wiring for [Saola](https://github.com/JorDunn/saola-panel),
a Linux desktop environment built in Rust and targeting the
[niri](https://github.com/YaLTeR/niri) compositor. One `systemd --user`
daemon, one binary, unifying three concerns that gate each other and share
one state machine:

1. **Idle policy** (`ext-idle-notify-v1`) — lock the session after N minutes
   idle, power off the outputs after M.
2. **Before-sleep locking** (logind) — hold a `sleep` delay inhibitor, catch
   `PrepareForSleep`, spawn the locker, confirm the lock actually took
   effect, release the inhibitor. Also honors logind's session `Lock` signal
   (`loginctl lock-session`), which niri does not consume on its own.
3. **An `org.freedesktop.ScreenSaver` D-Bus shim** — so apps that ask the
   desktop to suppress idle the freedesktop way (Firefox playing a video is
   the canonical case) actually work. On a stock niri session this module
   runs inert, because niri already implements this interface itself; the
   shim exists for any other compositor and as defense in depth.

This daemon has no UI and draws nothing — see
[`PLAN.md`](PLAN.md) for the full design rationale and
[`CLAUDE.md`](CLAUDE.md) for the binding engineering rules (no `panic!`/
`unwrap`/`expect` on any runtime path, the sudo rule, the severity order
below).

## Why one daemon and not three

The three concerns above gate each other: an idle-lock and a before-sleep
lock must never stack a second locker on top of a live one, and a
ScreenSaver inhibit must suppress idle actions but never the before-sleep
lock (a lid close mid-movie must still lock). Splitting them into separate
processes would need an IPC layer whose only job is reassembling state that
already lives naturally in one process.

## Failure severity order (binding — see `CLAUDE.md`)

Every design decision in this crate is ranked against this ordering, worst
first:

1. **The machine suspends or idles with the session unlocked/exposed.**
2. **Suspend is blocked or delayed indefinitely.**
3. Spurious lock.

When two behaviors conflict, the lower-numbered one always wins. logind
itself caps delay inhibitors (`InhibitDelayMaxUSec`, ~5s on a typical
machine), so "blocked forever" is systemically impossible regardless of what
this daemon does — the design works within that cap rather than fighting it.

## Build

```bash
cargo build --release
```

Standard `cargo build`/`cargo test`/`cargo clippy --all-targets -- -D
warnings`/`cargo fmt --check` — see `CLAUDE.md`'s Commands section. No
system libraries beyond what `zbus` and `wayland-client` already need
(pure-Rust D-Bus and Wayland backends — no `libdbus`, no direct
`libwayland-client.so` link requirement).

## Install

User-level throughout — nothing here ever needs `sudo`.

```bash
install -Dm755 target/release/saola-session ~/.local/bin/saola-session
install -Dm644 contrib/systemd/saola-session.service \
    ~/.config/systemd/user/saola-session.service
```

If you installed the binary somewhere other than `~/.local/bin` (which must
be on the `PATH` the user manager sees), edit `ExecStart=` in the copied
unit file to match — the unit as shipped in this repo points at
`/usr/bin/saola-session`, the path a distro package would use.

Then either enable the systemd unit (recommended — it gets
`Restart=on-failure` and the other fixes described in
[`contrib/systemd/saola-session.service`](contrib/systemd/saola-session.service)'s
own comments):

```bash
systemctl --user daemon-reload
systemctl --user enable --now saola-session.service
```

...or, for the simplest possible setup with no process-supervision benefits,
a `spawn-at-startup` line in `~/.config/niri/config.kdl` (the same mechanism
this machine already uses for `saola-panel`):

```kdl
spawn-at-startup "saola-session"
```

Prefer the systemd unit if you can: `Restart=on-failure` is what makes this
daemon self-healing after a crash, and `saola-panel`'s own README makes the
same recommendation for the same reason.

### Zero-touch install (the packaged path)

Everything above is the *development* install, and every manual step in it —
copying the unit, editing `ExecStart=`, fixing the user manager's `PATH`
(see Known limitations) — is an artifact of running out of `~/.local/bin`.
The packaged install ([`contrib/aur/PKGBUILD`](contrib/aur/PKGBUILD)) has
none of it, by design: install the package, log in, `loginctl lock-session`
works. Three mechanisms make that true, each solving one of the manual
steps:

1. **Binaries land in `/usr/bin`** — on the user manager's default `PATH`,
   so the shipped unit's `ExecStart=/usr/bin/saola-session` is already
   correct and the default `locker "saola-lockscreen"` resolves with no
   environment surgery.
2. **The package ships a static enablement symlink**
   (`/usr/lib/systemd/user/graphical-session.target.wants/`) — the same
   mechanism the big DEs use. systemd treats it exactly like
   `systemctl --user enable`, for every user on the machine, with no
   post-install commands. The matching
   `ConditionEnvironment=XDG_CURRENT_DESKTOP=niri` gate in the unit keeps
   it from also firing under GNOME/KDE on a dual-desktop machine, where a
   second sleep inhibitor, ScreenSaver shim, and locker would fight the
   resident DE (a failed condition is "skipped", not "failed").
3. **`WAYLAND_DISPLAY` comes from niri itself** — niri run as a proper
   session imports it into the user manager. A niri started some way that
   *doesn't* do that also never reaches `graphical-session.target`, so the
   unit consistently stays off rather than starting half-broken.

The upshot: the manual sections above exist for hacking on this repo; end
users of the Saola DE should only ever meet the package.

## Configuration — `~/.config/saola/session.toml`

Entirely optional; every knob has a built-in default; the file is read once
at startup (no live reload — a `systemctl --user restart` is cheap enough
that live-reload isn't worth the complexity). The daemon looks in
`$SAOLA_CONFIG_DIR`, then `$XDG_CONFIG_HOME/saola`, then `~/.config/saola`,
in that order (an empty environment variable is treated as unset, same as
the XDG spec's own rule). This is the same TOML format and the same search
order as the other Saola components.

```toml
# ~/.config/saola/session.toml — every key optional; omit the file for defaults.
locker = "saola-lockscreen"   # default; any command to spawn — no shell, plain argv
lock-before-sleep = true      # default

[idle]
lock-after-secs = 300         # whole seconds; omit to disable the idle lock
power-off-after-secs = 600    # whole seconds; omit to disable output power-off
```

Keep the two top-level keys **above** the `[idle]` header. In TOML, each key
that comes after a table header is part of that table. A `lock-before-sleep`
below `[idle]` is thus `idle.lock-before-sleep`, a key the daemon ignores.

| Knob | Default | Notes |
|---|---|---|
| `idle.lock-after-secs` | disabled | Idle timeout before the daemon spawns the locker, in whole seconds. Absent means the idle lock is **disabled**, not "some fallback timeout" — the daemon does not even open a Wayland connection if both `[idle]` timeouts are absent. |
| `idle.power-off-after-secs` | disabled | Same value form as `lock-after-secs`, and an independent timeout: one does not imply or disable the other. Runs `niri msg action power-off-monitors` on expiry. |
| `locker` | `"saola-lockscreen"` | The command spawned for every lock trigger (idle timeout, before-sleep, `loginctl lock-session`) — one spawn implementation, reused by all three. Resolved via `$PATH` at spawn time like any bare command. Split on whitespace only — no shell, no quoting, so a locker path that contains spaces is not expressible; this is deliberate (Architecture: a config file must not become a code-execution surface). |
| `lock-before-sleep` | `true` | Whether the before-sleep module spawns the locker on `PrepareForSleep`/`Lock` at all. |

**Resilience rule, stated once because it matters more than the schema
itself:** no malformed config can ever silence before-sleep locking. A
missing file, an unparseable document, or a single bad knob all fall back to
`lock_before_sleep: true` — never `false`. A bad config can silence
idle-lock (which is opt-in and defaults to disabled anyway) but never the
before-sleep concern, per the severity order above.

### Migrating from `session.kdl`

Version 0.1.0 and before used `~/.config/saola/session.kdl`. Each key keeps
its name in `session.toml`. There is one difference. The two idle timeouts
are in **seconds**, and their names include `-secs`. The unit is in the name
to make the change easy to see. In KDL, `lock-after 5` was five minutes. In
TOML, the same timeout is `lock-after-secs = 300`. The daemon no longer
accepts the `"30s"` and `"5m"` string forms — give the seconds as an integer.
A boolean is `true` or `false`, not KDL's `#true` or `#false`.

If the daemon finds a `session.kdl` but no `session.toml`, the daemon writes
one warning to the journal. The warning names the two paths. The daemon then
starts with its defaults. The daemon does not read the KDL file. Delete that
file after you copy its settings.

### Validating a config edit — `--check-config`

```bash
saola-session --check-config
```

Loads `session.toml` through the exact same loader the real daemon uses and
prints the resolved, effective config — including every per-knob fallback —
then exits. Any problem in the file (an unparseable document, a bad
`locker`, a `lock-after-secs` that isn't a positive whole number of seconds)
is printed as a warning on stderr before the config, so a bad edit shows up
as an explicit complaint rather than a silently-defaulted value you have to
infer. Set `RUST_LOG=debug` for more
detail; `RUST_LOG` unset defaults `--check-config` to `warn`-level output
(quieter than the real daemon's `info` default, since this is a
human-facing command meant to show only the config plus its complaints).

## Retiring `saola-lockscreen`'s interim session wiring

Before this daemon existed, `saola-lockscreen`'s `contrib/session/` directory
scaffolded the same three concerns with a shell script + `systemd --user`
unit for before-sleep locking, and a documented (but on this machine,
never-installed) recommendation to add `swayidle` for idle-lock. Its own
README says explicitly that this scaffolding exists only until a real
`saola-session` package does — that package is this one. If you followed
that README, retire its wiring now that this daemon covers the same ground
with the actual state machine Architecture specifies (shared already-locked
checks across idle/sleep, a real confirmation deadline, `KillMode=process`
so a daemon restart can't kill a lock screen it just spawned, and the rest
of `docs/REVIEW-v0.1.md`'s fixes) rather than a best-effort shell script.

```bash
# 1. Stop and disable the interim before-sleep unit.
systemctl --user disable --now saola-lock-before-sleep.service

# 2. Remove the interim script and unit file (installed by that repo's own
#    contrib/session/README.md instructions).
rm -f ~/.local/bin/saola-lock-before-sleep
rm -f ~/.config/systemd/user/saola-lock-before-sleep.service
systemctl --user daemon-reload
```

```bash
# 3. If you ever added the recommended swayidle spawn-at-startup line,
#    remove it from ~/.config/niri/config.kdl:
#      spawn-at-startup "swayidle" "-w" "timeout" "300" "saola-lockscreen"
# saola-session's own idle.lock-after-secs (session.toml, above) replaces it —
# swayidle itself can stay installed or be removed, your call; this daemon
# doesn't care either way, it just stops needing swayidle for locking.
```

```bash
# 4. Verify nothing from the old wiring is left running.
systemctl --user list-units 'saola-lock-before-sleep*'   # should be empty
pgrep -fa swayidle                                        # only relevant if you'd installed it
```

The manual lock keybind (`Mod+Escape` spawning `saola-lockscreen` directly)
and the PAM policy (`contrib/pam/saola-lockscreen`) are **not** part of this
retirement — this daemon does not touch either. Leave them exactly as
`saola-lockscreen`'s own README installed them.

### PR-ready note for `saola-lockscreen/contrib/session/README.md`

The text below is meant to be pasted into a PR against the sibling repo,
pointing its interim wiring at this package. **Not applied here** — this
repo never edits another repo's files; Jordan (or a future stage in that
repo) applies it there.

> **This directory is retired.** `saola-session`
> (https://github.com/JorDunn/saola-session) now owns idle policy,
> before-sleep locking and `loginctl lock-session` as a real daemon with a
> shared state machine across all three, replacing this directory's shell
> script + `systemd --user` unit + `swayidle` recommendation. See
> `saola-session`'s README for install instructions and the exact commands
> to retire this directory's wiring. The PAM policy
> (`contrib/pam/saola-lockscreen`) and the manual lock keybind are unaffected
> and stay exactly as documented elsewhere in this file.

## Known limitations

- **Lock confirmation is `LockedHint` polling, not a hard guarantee.**
  `docs/SIGNALS.md`'s research (Stage 2) established that logind's
  per-session `LockedHint` property — set by niri itself once its lock
  state reaches `Locked(_)` — is the best available confirmation signal, and
  confirmed it empirically on a real session. But the before-sleep path
  still has a hard deadline (a few seconds, derived from logind's own
  `InhibitDelayMaxUSec`): if confirmation hasn't arrived by then, the
  inhibitor is released and the suspend proceeds anyway, on the reasoning
  that the locker was at least started and logind would force the issue
  within a few more seconds regardless. A pathologically slow locker start
  (cold page cache, a stalled PAM/vault check) can in principle still lose
  that race. The daemon logs loudly (`lock NOT confirmed within the
  deadline`) whenever this happens — `journalctl --user -u saola-session -p
  warning` is where to look.
- **The logind side cannot be exercised in a nested niri.** A nested
  compositor is not a logind session — no `LockedHint`, no
  `PrepareForSleep`, nothing for `loginctl lock-session` to target. The
  before-sleep module (`sleep.rs`) is built to log-and-continue in that
  environment rather than crash (`CLAUDE.md`'s nested-niri testing rule),
  which is expected, not a bug, if you see it. One side effect: in a nested
  test, idle-lock spawns a fresh locker on every idle cycle rather than
  detecting an already-locked session (there is no `LockedHint` to read) —
  again expected in that environment only, never on a real session.
- **`active (running)` does not by itself prove the daemon is doing
  anything.** `systemd status` reports a process is alive, not that it
  holds a delay inhibitor or has a live logind connection. The unit
  self-heals from a startup-time logind outage (retried on a timer) and
  from a `systemd-logind` restart (detected via `NameOwnerChanged` and
  handled by dropping the stale inhibitor and re-acquiring), but the way to
  actually verify it is doing its job is:

  ```bash
  systemd-inhibit --list | grep saola-session
  # Expect a `sleep`/`delay` row, WHO=saola-session.
  ```

  An `active (running)` unit with nothing in that output means before-sleep
  locking is not currently protected — check
  `journalctl --user -u saola-session -p warning` for why.
- **The locker's environment matters.** The locker inherits this daemon's
  environment, unmodified. Under `systemd --user` that is the user
  manager's environment, which contains `WAYLAND_DISPLAY` only if the
  compositor imported it there. If it's absent, the locker fails to connect
  to a compositor and dies — the only visible symptom is the
  `lock NOT confirmed within the deadline` warning, since this daemon never
  reads the locker's own output (Architecture's spawn-hygiene rule). Verify
  `loginctl lock-session` actually locks the screen **under systemd**
  (`systemctl --user status`), not only under an interactive `cargo run`
  from a terminal, which inherits a complete environment and cannot
  reproduce this gap.

  The same applies to `PATH`, with a different (and louder) symptom: the
  user manager's default `PATH` is the system directories only — no
  `~/.local/bin` — so the default `locker "saola-lockscreen"` fails to
  spawn at all (`FAILED to spawn the locker … No such file or directory` in
  the journal; observed on this machine, 2026-08-03). The general fix is to
  put `~/.local/bin` on the user manager's `PATH`:

  ```bash
  systemctl --user import-environment PATH   # immediate, this login only
  mkdir -p ~/.config/environment.d           # persistent, from next login
  echo 'PATH=/home/jordan/.local/bin:$PATH' \
      > ~/.config/environment.d/50-local-bin.conf
  ```

  The narrow alternative is an absolute path in `session.toml`
  (`locker "/home/jordan/.local/bin/saola-lockscreen"`), but fixing the
  user manager's `PATH` also covers every other user unit that shells out
  to something in `~/.local/bin`.
- **A killed/crashed locker is not automatically re-spawned mid-session.**
  If the locker process dies while the compositor still considers the
  session locked (niri's own fallback per `saola-lockscreen`'s `CLAUDE.md`:
  a dead lock client leaves niri's own lock surface up, accepting no
  password), this daemon does not currently notice and re-spawn a
  replacement — only a fresh idle timeout, `Lock` signal, or
  `PrepareForSleep` triggers a new spawn attempt. `KillMode=process` in the
  shipped unit (see below) is what stops the *common* trigger for this
  (a daemon restart) from ever killing the locker in the first place; this
  limitation is about a locker that dies for some *other* reason.

## The systemd unit, and why it looks the way it does

[`contrib/systemd/saola-session.service`](contrib/systemd/saola-session.service)
is not systemd's defaults with a description slapped on — every non-default
line traces back to a specific finding in
[`docs/REVIEW-v0.1.md`](docs/REVIEW-v0.1.md) (Stage 7's exposure/silent-
failure audit), and the unit file's own comments name which one. In brief:

- `Restart=on-failure` with `RestartSec=2s` and `StartLimitIntervalSec=0` —
  this daemon's absence is a severity-1 condition, so it should retry
  forever rather than give up after systemd's default handful of fast
  restarts (which a lost boot-order race against the compositor could
  otherwise exhaust in under a second).
- `KillMode=process` — the single most important line in the file. systemd's
  default (`control-group`) kills every process in this unit's cgroup on
  every stop, including the stop half of a restart. This daemon spawns the
  locker as an ordinary child; without `KillMode=process`, restarting the
  daemon (for any of the reasons above) would also kill a lock screen it had
  just spawned, leaving niri's own no-password fallback surface up with no
  way in short of a VT switch.
- `After=graphical-session.target niri.service` / `PartOf=graphical-session.target`
  — ordered against what actually provides the Wayland socket, and torn
  down along with the graphical session rather than outliving it.

## Jordan-run end-to-end test sequence

**Nothing in this repo's stages, and nothing an agent working in this repo
does, ever runs `systemctl suspend` or suspends/locks a real session** — see
`CLAUDE.md`'s sudo rule and nested-niri testing rule. The sequence below is
for Jordan to run himself, in order, against the **fixed** code (this
stage's changes) — not the pre-review build. Two of `docs/REVIEW-v0.1.md`'s
findings (E-2's stale-inhibitor-after-a-logind-restart behavior, and I-6's
`WAYLAND_DISPLAY` environment requirement) can only be falsified by actually
running this sequence; everything upstream of it is read-only or unit-tested.

1. **Enable the unit** (after `cargo build --release` and the Install steps
   above):

   ```bash
   systemctl --user enable --now saola-session.service
   systemctl --user status saola-session.service
   ```

   Expect `active (running)` — but remember the known-limitations note
   above: this alone proves the process started, not that it's doing
   anything. Continue to step 2 before trusting it.

2. **Verify the delay inhibitor is actually held:**

   ```bash
   systemd-inhibit --list | grep saola-session
   ```

   Expect one row, `WHAT=sleep`, `MODE=delay`, `WHO=saola-session`,
   `WHY=Lock the session before sleep`. If this is empty, check
   `journalctl --user -u saola-session -p warning` — either logind was
   unreachable at startup (should self-heal within `RECONNECT_INTERVAL`,
   30s) or `lock-before-sleep = false` is set in `session.toml`.

3. **`loginctl lock-session`** — proves the `Lock` signal path and, per the
   known-limitations note above, must be checked with the daemon running
   under systemd (not `cargo run`) to actually exercise the environment gap
   I-6 describes:

   ```bash
   loginctl lock-session
   ```

   Expect the screen to lock within roughly the ~360ms `docs/SIGNALS.md`
   measured (warm). If nothing happens, check
   `journalctl --user -u saola-session` for a `FAILED to spawn the locker`
   (most likely `~/.local/bin` missing from the user manager's `PATH` —
   see the locker-environment bullet under Known limitations for the fix)
   or a Wayland-connect error (`WAYLAND_DISPLAY` missing from the user
   manager's environment, I-6).

4. **A short idle-lock timeout**, to prove the idle path independent of the
   sleep path:

   ```bash
   # Temporarily, in ~/.config/saola/session.toml:
   #   [idle]
   #   lock-after-secs = 20
   systemctl --user restart saola-session.service
   ```

   Stop touching the keyboard/mouse for 20 seconds. Expect the locker to
   spawn and lock the screen; unlock, wait past the timeout again with no
   activity, and confirm it fires again (the re-arm-on-`Resumed` behavior).
   Revert `session.toml` and restart the unit when done.

5. **Only after 1–4 all pass**, the real suspend/resume round trip — the one
   step this whole plan has deliberately never performed:

   ```bash
   systemctl suspend
   ```

   Close the lid or run the command above, then resume the machine. Expect
   the locker to be on screen **at the moment the display comes back** —
   not the desktop, even briefly. Check
   `journalctl --user -u saola-session` for `lock confirmed` (the happy
   path) versus `lock NOT confirmed within the deadline` (still safe — the
   locker was started, per the design — but worth knowing which one
   actually happened). Repeat at least once more to build confidence the
   result wasn't a one-off.

**Only after step 5 succeeds — twice, per the note above — is tagging
`0.1.0` Jordan's call to make.** That gate passed on 2026-08-05 (three
clean suspend round trips, lock confirmed before sleep each time, locker
on screen at resume) and `0.1.0` was tagged — see `CHANGELOG.md` and
`release-plz.toml`. The sequence above remains the recipe for verifying
any future release on real hardware.

## Credits

- [saola-lockscreen](https://github.com/JorDunn/saola-lockscreen) — the
  session locker this daemon spawns, and the closest sibling and convention
  source for this repo's own `CLAUDE.md` and testing rules. Its
  `contrib/session/` directory was the interim scaffolding this package
  replaces.
- [saola-panel](https://github.com/JorDunn/saola-panel) — source of this
  repo's layout conventions (`rust-toolchain.toml`, `rustfmt.toml`, the dual
  MIT/Apache license, `release-plz.toml`/`CHANGELOG.md` setup) and its own
  `contrib/systemd/saola-panel.service` as this daemon's unit-file template.
- [saola-theme](https://github.com/JorDunn/saola-theme) — the design system
  every other Saola component depends on. This daemon has no UI and,
  uniquely among Saola components, no dependency on it.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), your
choice.
