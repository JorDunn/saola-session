# Signal research: how do we know the session is locked?

Stage 2 output. Every claim below is either pasted command output from this
machine, a source-file citation (with the exact commit the installed niri
binary was built from), or a verbatim doc/man-page quote. Nothing here is
inference dressed up as fact — while evidence was outstanding it was marked
**PENDING (Jordan)**, not guessed; every such entry has since been resolved
with observed values (see the LockedHint observation table).

Probe environment: `nt-14589`, 2026-08-02/03. `niri --version` → `niri 26.04
(8ed0da4)`, running as the real session compositor (`ps aux` shows
`niri --session`, PID 1336, under `niri.service`, `XDG_SESSION_ID=3`).

Source clone: `https://github.com/YaLTeR/niri.git` into the scratchpad at
`niri-src`. `git log -1 --format='%h' v26.04` → `8ed0da44`, which is
byte-identical to the `(8ed0da4)` build id the installed binary reports —
**the clone is confirmed to be the exact source of the running binary**, not
just "close enough main branch." Full commit: `8ed0da44d974c32c6877d2f4630c314da0717ecb`.

---

## 1. Does niri set logind's per-session `LockedHint`?

**Yes — niri calls `Session.SetLockedHint` on lock/unlock transitions, and
this build has that code path compiled in and (per ambient evidence below)
apparently working on this machine.**

Source (`niri-src/src/niri.rs`, tag `v26.04` = `8ed0da44`):

```rust
// niri.rs:386-387
// State that we last sent to the logind LockedHint.
pub locked_hint: Option<bool>,
```

```rust
// niri.rs:5974 (fn signature) .. 6048
#[cfg(feature = "dbus")]
fn update_locked_hint(&mut self) {
    ...
    if !self.is_session_instance {
        return;
    }
    static XDG_SESSION_ID: LazyLock<Option<String>> = LazyLock::new(|| {
        let id = std::env::var("XDG_SESSION_ID").ok();
        if id.is_none() {
            warn!("env var 'XDG_SESSION_ID' is unset or invalid; logind LockedHint won't be set");
        }
        id
    });
    let Some(session_id) = &*XDG_SESSION_ID else { return; };

    fn call(session_id: &str, locked: bool) -> anyhow::Result<()> {
        let conn = zbus::blocking::Connection::system()...;
        // GetSession(session_id) -> session_path
        // then call_method(session_path, "org.freedesktop.login1.Session", "SetLockedHint", &(locked))
    }

    // Consider only the fully locked state here. When using the locked hint with sleep
    // inhibitor tools, we want to allow sleep only after the screens are fully cleared with
    // the lock screen, which corresponds to the Locked state.
    let locked = matches!(self.lock_state, LockState::Locked(_));

    if self.locked_hint.is_some_and(|h| h == locked) { return; }  // dedup, only calls on transition
    self.locked_hint = Some(locked);
    thread::Builder::new().name("Logind LockedHint Updater".to_owned()).spawn(move || {
        if let Err(err) = call(session_id, locked) {
            warn!("failed to set logind LockedHint: {err:?}");
        }
    });
}
```

Call site (every redraw-loop iteration, but deduplicated internally so the
D-Bus call only fires on a real transition):

```rust
// niri.rs:774-775
#[cfg(feature = "dbus")]
self.niri.update_locked_hint();
```

Three preconditions verified for *this* machine, not assumed:

- **`feature = "dbus"` is compiled in.** `Cargo.toml:132`: `default =
  ["dbus", "systemd", "xdp-gnome-screencast"]` — it's a default feature, and
  nothing about the installed package suggests a custom feature set.
- **`is_session_instance` is true.** It's set from `cli.session` (`src/cli.rs:28`,
  the `--session` flag; wired through in `src/main.rs:73,184,215,229,232`).
  `ps aux` on this machine shows the running compositor's command line is
  literally `niri --session` (PID 1336, under `niri.service`) — confirmed
  by direct process inspection, not inferred from the systemd unit file.
- **`XDG_SESSION_ID` is set.** A shell in this same login session reports
  `XDG_SESSION_ID=3`, and `loginctl session-status 3` confirms it's the
  active `niri --session` Wayland session on seat0/tty1 (`Service=greetd`,
  `Type=wayland`, `Active=yes`).

`fn lock()` (`niri.rs:5843`) is only reached from the real
`ext-session-lock-v1` grant path (`LockState::Locking` →
`LockState::Locked(ExtSessionLockV1)` once the lock surfaces are up), so
`update_locked_hint`'s `matches!(self.lock_state, LockState::Locked(_))`
check means **the hint flips to `true` only once the lock surfaces are
actually confirmed rendered** — i.e. it tracks the same "fully locked, not
just locking" state the lockscreen's own `ext_session_lock_v1` `locked`
event would report if the lockscreen crate consumed it (which, per
`REVIEW-v0.1.md` finding L-3, it currently doesn't — this is exactly why
`LockedHint` is the candidate replacement signal).

### Ambient/historical evidence (not a live Jordan-directed test — superseded by the confirmed observation below)

This machine's `niri.service` journal already shows **six real-session
lock/unlock cycles tonight**, none of them run by this agent:

```
$ journalctl --user -u niri.service --no-pager -o cat | grep -iE "lock|LockedHint|XDG_SESSION_ID"
...
2026-08-02T23:30:39.086398Z  WARN niri::utils::spawning::systemd: error spawning "saola-lockscreen": Os { code: 2, kind: NotFound, message: "No such file or directory" }
2026-08-02T23:32:45.888576Z  INFO niri::niri: locking session
2026-08-02T23:33:00.858650Z  INFO niri::niri: unlocking session
2026-08-02T23:45:05.324672Z  INFO niri::niri: locking session
2026-08-02T23:45:20.690770Z  INFO niri::niri: unlocking session
2026-08-02T23:52:12.056903Z  INFO niri::niri: locking session
2026-08-02T23:52:26.170047Z  INFO niri::niri: unlocking session
2026-08-02T23:52:29.023694Z  INFO niri::niri: locking session
2026-08-02T23:53:20.851718Z  INFO niri::niri: unlocking session
2026-08-02T23:54:12.651367Z  INFO niri::niri: locking session
2026-08-02T23:54:22.218136Z  INFO niri::niri: unlocking session
```

Zero occurrences of the `warn!("failed to set logind LockedHint: {err:?}")`
or `warn!("error spawning a thread to set logind LockedHint...")` lines
across all six cycles — the only way `update_locked_hint`'s D-Bus call fails
silently to this journal is if it never errors, since both failure branches
are `warn!`-logged and `journalctl -u niri.service` would show them.

A read-only property read taken *after* this session, at rest (last logged
event was `unlocking session` at 23:54:22):

```
$ loginctl show-session $XDG_SESSION_ID | grep -i locked
LockedHint=no
```

This is consistent with — but does not by itself *prove* — `LockedHint`
tracking the lock state, since I only sampled it once, at a moment that
happens to match "unlocked." I did not sample it *during* one of tonight's
six lock windows (I wasn't invoked yet, and per this stage's ground rules I
do not lock the session myself to manufacture that sample).

### CONFIRMED — Jordan's real-session observation (2026-08-02, collected live)

Collection method: the orchestrator spawned `saola-lockscreen` from the
session shell with Jordan present at the machine, then polled `busctl
get-property org.freedesktop.login1 /org/freedesktop/login1/session/auto
org.freedesktop.login1.Session LockedHint` every ~0.5s while Jordan
performed a real unlock with his password. `niri.service`'s journal was
cross-checked for the same window.

**Result: confirmed.** Full transition timeline (timestamps corrected after
an initial measurement-error episode — see the note below the table):

| Time (local) | Event |
|---|---|
| 21:46:06 | Poller/monitor armed; baseline poll `LockedHint` = `b false` (session not yet locked) |
| 21:46:41.063 | `saola-lockscreen` spawned (birth timestamp of the orchestrator's background-task output file, created at the moment the spawn command executed) |
| 21:46:41.109 | niri journal: `INFO niri::niri: locking session` — **46ms after spawn** |
| 21:46:41.421 | Poller observes `LockedHint` = `b true` — **~360ms after spawn**, upper-bounded by the poller's ~0.57s sampling interval (i.e. the real transition could be anywhere in that ~360ms window, but not later) |
| 21:46:41 → 21:51:06 | 468 consecutive polls, all `b true` (~5 minutes of real lock) |
| 21:51:06–21:51:59 | Sampling gap (poller's own re-arm, not a niri/logind gap) |
| 21:51:41.610 | niri journal: `INFO niri::niri: unlocking session` |
| 21:51:59 (first sample after the gap) | `LockedHint` = `b false` — no evidence of lag across the gap |

The locker process exited `0` at unlock, as its design intends.
`journalctl --user -u niri.service` for the entire window shows **only**
the two `INFO` lines above matching `/lock|hint/i` — zero
`failed to set logind LockedHint` warnings. **The primary-signal hypothesis
below is not overturned; it is now empirically confirmed**, not just
ambient/historical evidence.

**Measurement-error note, kept for the record as a caution about wall-clock
inference:** an earlier pass through this data read the ~35-second gap
between the poller being armed (21:46:06) and the lock actually happening
(21:46:41) as if it were the locker's own spawn-to-lock latency, and wrote
that up as "~35s cold-start latency." That was wrong — the 21:46:06
timestamp was when the *monitor* was armed, not when the *spawn command*
was issued; the spawn didn't actually happen until 21:46:41 (message
composition and tool round-trip time in the orchestrating session, not
locker startup time at all). The corrected, precisely-timestamped numbers
are in the table above: **spawn→compositor-lock ≈ 46ms, spawn→LockedHint-
observed ≈ 360ms (poll-granularity-bounded)**. The locker is effectively
instant — consistent with the lockscreen's own H-1 review fix, which moved
wallpaper decode off the pre-lock path specifically to eliminate this kind
of startup delay. See the decision section for the corrected Stage 4
implication.

---

## 2. Does niri's IPC event stream expose lock state?

**No.** Exhaustively enumerated — `niri-ipc/src/lib.rs:1605` `pub enum
Event` has exactly these 18 variants, and none of them is lock-related:

```
WorkspacesChanged, WorkspaceUrgencyChanged, WorkspaceActivated,
WorkspaceActiveWindowChanged, WindowsChanged, WindowOpenedOrChanged,
WindowClosed, WindowFocusChanged, WindowFocusTimestampChanged,
WindowUrgencyChanged, WindowLayoutsChanged, KeyboardLayoutsChanged,
KeyboardLayoutSwitched, OverviewOpenedOrClosed, ConfigLoaded,
ScreenshotCaptured, CastsChanged, CastStartedOrChanged, CastStopped
```

(`grep -rn "Lock\|Session" niri-ipc/src/lib.rs` returns nothing outside
workspace/window field names — no `Locked`/`SessionLocked`/etc. variant
exists anywhere in the IPC crate.)

Confirmed live against the real socket too — `niri msg event-stream` was run
for ~6s against the real running compositor (read-only; the stream only
*receives*, sends nothing) and produced only `WorkspacesChanged`,
`WindowsChanged`, `KeyboardLayoutsChanged`, `OverviewOpenedOrChanged`,
`ConfigLoaded`, `CastsChanged`, `WindowOpenedOrChanged` — consistent with
the enum's contents and zero lock-related events, steady state.

`niri msg --help` and `Sub`/`Msg` in `src/cli.rs:38-100` were also checked
for a poll-style query (the way `niri msg outputs`/`windows`/`layers` work)
— there is no `niri msg lock-state` or equivalent. The lockscreen's own
`CLAUDE.md` already noted this from the locker's side ("There is no `niri
msg` query for 'is the session locked'" — `ext-session-lock-v1` never shows
up in `niri msg layers`/`windows`); this stage confirms it from the daemon
side too, across both the query and the push/event-stream surfaces.

**Conclusion: the IPC stream is not a candidate signal at all, primary or
fallback — the vocabulary simply does not contain lock state.** This
narrows Stage 4's options to `LockedHint` (primary candidate, evidence
above) and the logind `Lock`/`Unlock` session signals plus process-liveness
(see §3 and the decision section).

---

## 3. logind mechanics, verified not assumed

All read-only: `busctl introspect`/`get-property`, local D-Bus interface XML
under `/usr/share/dbus-1/interfaces/`, and `man org.freedesktop.login1`
(`systemd 261 (261.1-1-arch)` on this machine). No `loginctl lock-session`
was run.

### `Inhibit()` call shape and fd semantics

```
$ busctl introspect org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager | grep -i inhibit
.Inhibit                          method   ssss                h    -
```

`/usr/share/dbus-1/interfaces/org.freedesktop.login1.Manager.xml`:

```xml
<method name="Inhibit">
  <arg type="s" name="what" direction="in"/>
  <arg type="s" name="who" direction="in"/>
  <arg type="s" name="why" direction="in"/>
  <arg type="s" name="mode" direction="in"/>
  <arg type="h" name="pipe_fd" direction="out"/>
</method>
```

`man org.freedesktop.login1` (verbatim):

> Inhibit() creates an inhibition lock. It takes four parameters: what, who,
> why, and mode. what is one or more of "shutdown", "sleep", "idle",
> "handle-power-key", "handle-suspend-key", "handle-hibernate-key",
> "handle-lid-switch", separated by colons... who should be a short
> human-readable string identifying the application taking the lock. why
> should be a short human-readable string identifying the reason why the
> lock is taken. Finally, mode is either "block" or "delay" which encodes
> whether the inhibit shall be considered mandatory or whether it should
> just delay the operation to a certain maximum time... The method returns
> a file descriptor. **The lock is released the moment this file descriptor
> and all its duplicates are closed.**

So: `Inhibit("sleep", "saola-session", "<why>", "delay") -> fd`; release =
drop the fd (matches Architecture's "releasing = dropping the fd" exactly).
No explicit release call exists or is needed.

### `PrepareForSleep` direction and ordering

```
.PrepareForSleep    signal    b    -
```

`man org.freedesktop.login1` (verbatim):

> The PrepareForShutdown(), PrepareForShutdownWithMetadata(), and
> PrepareForSleep() signals are sent right before (with the argument "true")
> or after (with the argument "false") the system goes down for
> reboot/poweroff and suspend/hibernate, respectively. This may be used by
> applications to save data on disk, release memory, or do other jobs that
> should be done shortly before shutdown/sleep, **in conjunction with delay
> inhibitor locks**. After completion of this work they should release
> their inhibition locks in order to not delay the operation any further.

Direction: emitted **by logind, to subscribers** (not something a client
calls) — `true` before sleep, `false` after resume. This matches
Architecture's state machine exactly: `PrepareForSleep(true)` →
`LockPending`, `PrepareForSleep(false)` → re-acquire inhibitor.

### `InhibitDelayMaxUSec` on this machine

```
$ busctl get-property org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager InhibitDelayMaxUSec
t 5000000
```

**5,000,000 µs = 5 seconds**, confirming the CLAUDE.md's "typically ~5s"
note as this machine's actual value, not an assumption. `InhibitorsMax` is
`8192` (`const`). Stage 4's hard deadline for `LockPending` must be
comfortably under 5s (e.g. 2-3s) to guarantee release-before-logind-forces-it
in the worst case, per Architecture's binding severity rule 2.

### Session `Lock`/`Unlock`

```
$ busctl introspect org.freedesktop.login1 /org/freedesktop/login1/session/_33 | grep -iE "lock|active|state"
.Lock            method    -    -       -
.SetLockedHint   method    b    -       -
.Unlock          method    -    -       -
.Active          property b    true    emits-change
.CanLock         property b    true    const
.LockedHint      property b    false   emits-change
.State           property s    "active" emits-change
.Lock            signal    -    -       -
.Unlock          signal    -    -       -
```

(`/org/freedesktop/login1/session/_33` is this shell's own session, id `3`,
obtained via `Manager.GetSession("3")` — read-only.)

`man org.freedesktop.login1` (verbatim, both quotes load-bearing):

> Terminate(), Activate(), Lock(), Unlock(), and Kill() work similarly to
> the respective calls on the Manager object.
>
> Lock()/Unlock() is sent when the session is asked to be
> screen-locked/unlocked. **A session manager of the session should listen
> to this signal and act accordingly.** This signal is sent out as a result
> of the Lock() and Unlock() methods, respectively.
>
> Signals are only emitted on objects referencing a specific session ID,
> not on the "/org/freedesktop/login1/session/self" or
> "/org/freedesktop/login1/session/auto" convenience objects.

Two consequences, verified against niri's own source rather than assumed:

1. **niri does not itself listen for the `Lock`/`Unlock` signal.**
   `grep -rn "\"Lock\"\|\"Unlock\"\|login1.Session" niri-src/src/` finds
   exactly one match, and it's the *outbound* `SetLockedHint` call
   (`niri.rs:6017`), not an inbound signal subscription. This means
   `loginctl lock-session` today does **not** cause niri to lock anything —
   confirming Architecture's "logind's session `Lock` signal → same
   spawn-if-not-locked path" is load-bearing and not already handled
   upstream: **saola-session's `sleep.rs` is the thing that must listen for
   this signal and spawn the locker**, exactly as planned.
2. **Subscribe to the specific session object path** (e.g.
   `/org/freedesktop/login1/session/_33`, obtained once via `GetSession` at
   startup using `$XDG_SESSION_ID`), never `/session/self` or `/session/auto`
   — the man page is explicit that those convenience paths never emit
   signals.

`SetLockedHint()`'s own doc (verbatim, corroborating §1):

> SetLockedHint() may be used to set the "locked hint" to locked, i.e.
> information whether the session is locked. This is intended to be used by
> **the desktop environment** to tell systemd-logind when the session is
> locked and unlocked.

i.e. niri calling this is exactly its documented intended use, not a hack.

### What `loginctl lock-session` emits — from docs, not by running it

`man loginctl` (verbatim):

> lock-session [ID...], unlock-session [ID...]
>     Activates/deactivates the screen lock on one or more sessions, if the
>     session supports it.

Combined with the Session interface doc above: `loginctl lock-session`
resolves the session, then calls `Session.Lock()`, which logind turns
around and re-emits as the `Lock` **signal** on that session's object path
for listeners to act on. This was not run — the mechanism is established
purely from `man loginctl` + `man org.freedesktop.login1` +
`busctl introspect`.

---

## 4. `org.freedesktop.ScreenSaver` ownership

**Surprise, load-bearing for Stage 6's scope — not just "who owns the
name": niri itself already fully implements the `org.freedesktop.ScreenSaver`
shim, and its D-Bus inhibits already gate niri's own `ext-idle-notify-v1`
emission at the compositor level.**

```
$ busctl --user list | grep -i screen
org.freedesktop.ScreenSaver                  1336 niri  jordan :1.16  user@1000.service - -
org.gnome.Mutter.ScreenCast                  1336 niri  jordan :1.19  user@1000.service - -
org.gnome.Shell.Screenshot                   1336 niri  jordan :1.17  user@1000.service - -

$ busctl --user status org.freedesktop.ScreenSaver
PID=1336 ... Comm=niri ... CommandLine=niri --session ... UserUnit=niri.service
```

**PID 1336 is niri itself** (same PID as the compositor process confirmed in
§1's `ps aux`), not xdg-desktop-portal or any other daemon.
`xdg-desktop-portal`, `xdg-desktop-portal-gtk`, and
`xdg-desktop-portal-wlr` are all installed and running
(`systemctl --user list-units | grep portal` shows all three `active
running`), but none of them registers `org.freedesktop.ScreenSaver` — that
name is niri's, confirmed by the absence of any portal PID in the
`busctl --user list` output above and by reading niri's own source:

`niri-src/src/dbus/freedesktop_screensaver.rs` (177 lines, read in full)
implements exactly the `Inhibit(app, reason) -> cookie` /
`UnInhibit(cookie)` pair Architecture's Stage 6 spec describes, **including
the peer-vanish cleanup Stage 6 was told to implement**:

```rust
// freedesktop_screensaver.rs:94-129
async fn monitor_disappeared_clients(...) {
    let proxy = fdo::DBusProxy::new(conn)...;
    let mut stream = proxy.receive_name_owner_changed_with_args(&[(2, UniqueName::null_value())]).await...;
    while let Some(signal) = stream.next().await {
        ...
        if args.new_owner().is_none() {
            // peer's unique name dropped off the bus -> drop its cookies
            inhibitors.retain(|_, owner| owner != name);
            is_inhibited.store(!inhibitors.is_empty(), Ordering::SeqCst);
        }
    }
}
```

And registration (`freedesktop_screensaver.rs:138-152`) explicitly opts
into being replaceable:

```rust
let flags = RequestNameFlags::AllowReplacement
    | RequestNameFlags::ReplaceExisting
    | RequestNameFlags::DoNotQueue;
conn.object_server().at("/org/freedesktop/ScreenSaver", self.clone())?;
conn.object_server().at("/ScreenSaver", self)?;
conn.request_name_with_flags("org.freedesktop.ScreenSaver", flags)?;
```

**The critical wiring** — this is not a decorative shim; niri's own idle
policy already consumes it:

```rust
// niri.rs:335
pub is_fdo_idle_inhibited: Arc<AtomicBool>,

// dbus/mod.rs:89-90
let screen_saver = ScreenSaver::new(niri.is_fdo_idle_inhibited.clone());
dbus.conn_screen_saver = try_start(screen_saver);

// niri.rs:4006-4017
pub fn refresh_idle_inhibit(&mut self) {
    self.idle_inhibiting_surfaces.retain(|s| s.is_alive());
    let is_inhibited = self.is_fdo_idle_inhibited.load(Ordering::SeqCst)
        || self.idle_inhibiting_surfaces.iter().any(|surface| { ... });  // native zwp_idle_inhibit_manager_v1 surfaces
    self.idle_notifier_state.set_is_inhibited(is_inhibited);
}
```

i.e. **both** the D-Bus `org.freedesktop.ScreenSaver` inhibit path *and* the
native Wayland `zwp_idle_inhibit_manager_v1` surface-inhibit path (also
advertised in the nested-niri global list, §5) already feed into
`idle_notifier_state`'s suppression, which happens **inside niri, before any
`ext-idle-notify-v1` notification is ever sent to a client**.

### Coexistence story Stage 6 must follow

Because Architecture already commits saola-session's idle module (Stage 5)
to consuming `ext-idle-notify-v1` as its lock/power-off timer signal, and
niri already suppresses that protocol's notifications whenever *any* app
(Firefox included, via freedesktop ScreenSaver) or *any* surface (via native
idle-inhibit) is active — **the Firefox-video-inhibits-idle case Architecture
describes as Stage 6's whole reason to exist is already handled, for free,
by niri, with zero code in this daemon.**

Stage 6 must not attempt to claim `org.freedesktop.ScreenSaver` itself. Two
reasons, in Architecture's own severity order:

1. **It would actively break the thing it's trying to fix.** niri's
   `RequestNameFlags::AllowReplacement` means a second `request_name` call
   with `ReplaceExisting` *would* succeed in taking the name from niri. But
   then apps like Firefox would inhibit saola-session's shim instead of
   niri's — and niri's `is_fdo_idle_inhibited` would go stale (no more
   `Inhibit` calls reach it), silently breaking niri's own
   `ext-idle-notify-v1` suppression for **every** consumer of that protocol,
   not just this daemon. That is a severity-rule-1 regression (idle actions
   firing during an inhibit that used to work) introduced by the "fix."
2. **It would be pure duplication even if done safely** — two independent
   cookie-tracking implementations for the same apps to talk to, with no
   way for saola-session to know which one Firefox picked.

Stage 6's real job, given this finding, is almost certainly **not** "build
a competing shim" but "verify niri's existing suppression is sufficient and
either skip `inhibit.rs`'s D-Bus service entirely or repurpose it as a
read-only observer" — that scope decision belongs to Jordan/the
orchestrator, not this stage, but it must be flagged loudly before Stage 6
starts, since building the originally-planned shim would be a regression,
not a feature.

(Also checked and confirmed *not* relevant to the ownership question:
`org.freedesktop.impl.portal.desktop.wlr` shows `(activatable)` — not
currently running — and none of the running portal names is
`org.freedesktop.ScreenSaver`; portals implement `org.freedesktop.portal.*`
names, a different namespace entirely.)

---

## 5. `ext-idle-notify-v1` in nested niri

**Confirmed advertised.** Procedure followed exactly per the lockscreen
`CLAUDE.md`'s nested-niri rule (no `--session`, throwaway empty config,
explicit `NIRI_SOCKET`/`WAYLAND_DISPLAY` override so the outer real session
is never touched):

```
$ touch scratchpad/nested-niri.kdl
$ niri -c scratchpad/nested-niri.kdl &
...
listening on Wayland socket: wayland-2
IPC listening on: /run/user/1000/niri.wayland-2.51295.sock
```

`wayland-info` (the tool the plan suggested) is **not installed** on this
machine (`wayland-utils` package, `pacman -Ss` confirms it exists in `extra`
but `pacman -Q` shows it's not installed) and installing it would need
`sudo pacman -S`, which this stage may not run. Per the plan's own "or
equivalent" allowance, an equivalent was built instead of guessing: a
throwaway Rust binary (`scratchpad/list-globals`, using the exact
`wayland-client = "0.31.15"` version Stage 1 already pinned for this repo)
that does a `wl_registry` roundtrip and prints every advertised global.
Full source is in `scratchpad/list-globals/src/main.rs`; ~25 lines,
`Dispatch<WlRegistry, ()>` collecting `Global { interface, version, .. }`
events.

Run against the nested instance only (`WAYLAND_DISPLAY=wayland-2`, **not**
the real session's `wayland-1`):

```
15: ext_idle_notifier_v1 v2
7: ext_session_lock_manager_v1 v1
16: zwp_idle_inhibit_manager_v1 v1
... (40 globals total)
```

`ext_idle_notifier_v1` (version 2) **is** advertised by the nested instance.
`zwp_idle_inhibit_manager_v1` (the native surface-based inhibit protocol
consumed by `refresh_idle_inhibit`, §4) is present too. Stage 5's nested-niri
live-test plan (register `ext-idle-notify-v1`, fire short timeouts) is
viable exactly as planned.

The nested niri instance was killed immediately after
(`pkill -f "niri -c scratchpad/nested-niri.kdl"`); confirmed gone via `ps aux`.

---

## Decision section

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
spawn→`LockedHint`-observed ≈ 360ms** (poll-granularity-bounded; see §1's
corrected timeline and its measurement-error note — an earlier pass
mis-attributed an orchestrator-side 35-second message/tool round-trip delay
to locker startup, which was wrong and has been corrected). On this warm,
already-primed system, `LockPending`'s confirmation arrives **well within**
the 5s `InhibitDelayMaxUSec` window (§3) — the deadline-then-proceed path
is not the routine outcome here.

That said, **Architecture's deadline design remains mandatory, not
optional polish**: one warm-system sample does not bound the worst case.
Cold caches, a resume-time PAM/vault stall, disk pressure, or a locker
version regression could all push spawn-to-lock past the 5s budget, and
logind's own cap means the daemon must still release-and-proceed on
deadline regardless of how fast the happy path measured here. Stage 4
should implement the deadline exactly as Architecture specifies and log
the spawn-to-confirmation duration on every cycle — cheap, and it turns
this "should be fast" belief into an ongoing measurement instead of a
one-time assumption.

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

---

## Surprises for the record

1. **The ambient lock/unlock cycles, later superseded by the controlled
   observation.** This machine had already run six real-session lock/unlock
   cycles before this stage started (journal timestamps 23:32-23:54 the
   night this stage began), including one failed locker-spawn attempt at
   23:30:39 (`saola-lockscreen` not found — likely a not-yet-installed/PATH
   issue on Jordan's end, unrelated to this stage). That ambient evidence
   has since been superseded by the controlled, Jordan-witnessed
   observation recorded in §1 — the ambient section is kept for the
   record but the confirmed section is now authoritative. `CLAUDE.md`'s
   "first real-session lock is still pending" note is now stale (multiple
   real locks have happened); Stage 8 should double check with Jordan
   whether that note needs updating too.
2. **niri's `LockedHint` update runs on a spawned OS thread per transition**
   (`thread::Builder::new().spawn(...)`, not spawn_blocking on an existing
   pool) — not relevant to how saola-session reads it (it's a plain D-Bus
   property from the reader's side) but worth knowing if timing ever looks
   surprising: the hint update is not synchronous with the redraw loop that
   triggers it.
3. **The `org.freedesktop.ScreenSaver` finding (§4) is bigger than a
   "who owns the name" answer** — it's a scope question for Stage 6 that
   should be resolved with Jordan before that stage starts building.
4. **A "35s cold-start latency" figure was initially reported and was
   wrong** — it conflated orchestrator-side message/tool round-trip delay
   (monitor armed at 21:46:06, spawn command not actually issued until
   21:46:41) with locker startup time. The corrected, precisely-timestamped
   measurement is spawn→compositor-lock ≈ 46ms, spawn→`LockedHint`-observed
   ≈ 360ms (poll-bounded) — see §1 and the decision section. Confirmation
   arrives well inside the 5s `InhibitDelayMaxUSec` window on this warm
   system; the deadline-then-proceed path is a defensive necessity for
   pathological cases, not the routine outcome. Kept here as a caution
   about trusting wall-clock deltas across process boundaries without
   pinning down what each timestamp actually measures.
5. **Live interim wiring confirmed still active on this machine** during
   the same observation window: `swayidle -w timeout 300 saola-lockscreen`
   (PID 1415) and the lockscreen contrib's
   `systemd-inhibit --what=sleep --mode=delay --who=saola-lockscreen …
   saola-lock-before-sleep --once` unit are both running. This is exactly
   the `contrib/session/` scaffolding Architecture says this daemon exists
   to replace (see PLAN.md's opening context) — both must be retired when
   this daemon ships. Recorded here for Stage 8's retirement-instructions
   section; not this stage's job to remove.

---

## Jordan's real-session observation — resolved (was "NEEDED FROM JORDAN")

The two required commands were collected live by the orchestrator with
Jordan at the machine (method, full timeline, and journal cross-check are
in §1's "CONFIRMED" subsection above). Both expected outcomes were met:
`LockedHint = b true` throughout a real lock (468 samples), `LockedHint = b
false` after a real password unlock, zero `SetLockedHint` failure warnings
in the journal for the whole window. No further Jordan action is needed for
this stage's signal question. An initial "~35s cold-start latency" figure
derived from this same observation was later found to be a measurement
error (orchestrator-side round-trip delay, not locker startup — item 4
above) and has been corrected throughout this document; the accurate
numbers are spawn→compositor-lock ≈ 46ms, spawn→`LockedHint`-observed
≈ 360ms.
