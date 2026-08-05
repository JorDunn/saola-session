# Stage 6 handoff — ScreenSaver inhibit shim (`src/modules/inhibit.rs`)

Compressed state for Stage 7 (read-only exposure/silent-failure audit of the
whole crate). Everything below is as-built and verified: `cargo build &&
cargo clippy --all-targets -- -D warnings && cargo test` exit 0, `cargo fmt
--check` clean, **76 tests total** (62 going in per Stage 5's handoff, 14 new
here, 0 removed).

## Name-ownership behavior as shipped

Jordan's binding resolution (2026-08-03, recorded in Stage 5's handoff):
build the full shim, but never blindly claim the name. As built:

1. Connect to the session bus (`zbus::Connection::session()`). Failure here
   degrades to inert (log, hold `inhibit_tx` alive, wait for shutdown) —
   never fatal to the process.
2. Register the `ScreenSaverInterface` at **both**
   `/org/freedesktop/ScreenSaver` (canonical) and `/ScreenSaver` (legacy
   alias niri also serves, per `docs/SIGNALS.md` §4's source excerpt) —
   *before* attempting the name claim, per `zbus::Connection::
   request_name_with_flags`'s own doc ("set up your service implementation
   ... **after**"). The canonical-path registration failing is fatal to
   serving (inert); the legacy-path failing is only a warning (continues
   with just the canonical path).
3. One conditional claim attempt (`ZbusNameClaimant::try_claim`):
   `request_name_with_flags(SCREEN_SAVER_NAME, RequestNameFlags::DoNotQueue.
   into())` — **`DoNotQueue` alone**, no `AllowReplacement`, no
   `ReplaceExisting`. This is the load-bearing bit Stage 2/5 called out:
   niri registers its own shim with `AllowReplacement` set, so a claim that
   also set `ReplaceExisting` would *succeed* and silently steal the name
   from niri, breaking niri's own `ext-idle-notify-v1` suppression for every
   other app. Without `ReplaceExisting`, `zbus` maps an already-owned name to
   `Err(zbus::Error::NameTaken)` regardless of the *current* owner's flags —
   verified this is exactly what happens against real niri (see live
   evidence below).
4. `Ok(ClaimOutcome::AlreadyOwned)` (or a claim error) → `ServiceMode::Inert`
   — hold `inhibit_tx` alive, never send, wait for shutdown. The channel's
   initial `false` (Stage 5's default) is already correct, so doing nothing
   here is exactly right, not a stopgap.
5. `Ok(ClaimOutcome::Claimed)` → `ServiceMode::Active` — build a
   `zbus::fdo::DBusProxy`, subscribe to `NameOwnerChanged` filtered to
   `(2, "")` (peer disconnected), and run the peer-vanish watcher until
   shutdown. If the `DBusProxy` itself fails to build, the module still
   waits for shutdown rather than returning (returning early would drop the
   `#[must_use]` `Connection`, which would silently stop serving
   Inhibit/UnInhibit too — worse than just losing peer-vanish cleanup).

**Real-machine result** (both the inert path, live, and the active path,
live — see "Live-check results" below): on Jordan's actual session, niri
(PID 1336) owns `org.freedesktop.ScreenSaver`, so the shim stays inert
there, exactly as Stage 2 predicted. It only goes active under a bus where
nothing owns the name (verified via `dbus-run-session`).

## Cookie semantics (`InhibitStore`, pure, no I/O)

- `inhibit(peer, app, reason) -> (cookie, ActiveTransition)`. Cookie
  allocation is `next_cookie: u32` with `wrapping_add(1)` (no-panic rule —
  overflow after 4 billion concurrent grants is type-possible even if
  practically absurd; wrapping means a *theoretical* future collision would
  silently overwrite via `HashMap::insert` rather than panic — flagged
  explicitly in the module doc comment for Stage 7 to weigh in on if it
  disagrees with "out of scope").
- `uninhibit(cookie) -> (found: bool, ActiveTransition)`. **Double or
  unknown-cookie `UnInhibit` is not an error** — `found = false`,
  `Unchanged`, logged at `debug!` only (per the freedesktop spec's
  fire-and-forget cookie model and Stage 6's own task list, which names this
  case explicitly).
- `peer_vanished(peer) -> (Vec<Grant>, ActiveTransition)` — called only from
  the `NameOwnerChanged` watcher, never from `Inhibit`/`UnInhibit` directly.
  Returns the actual dropped `Grant`s (not just a count) specifically so the
  caller's log line can name the crashed app and its stated reason —
  verified live (see below): the log line reads `grants="saola-live-test
  (verifying Stage 6 active path)"`.
- `ActiveTransition` (`Unchanged`/`BecameActive`/`BecameInactive`) is
  computed *inside* `InhibitStore`'s methods (before/after `is_active()`),
  not re-derived by the caller — this is what makes "send on zero↔nonzero
  transitions only, not every grant/release" (Stage 5's handoff wording)
  correct by construction rather than by a caller convention that could
  drift. Proven by `a_second_concurrent_inhibit_does_not_resend`.
- `Shared` (D-Bus-adjacent but itself D-Bus-free: `Mutex<InhibitStore>` +
  `watch::Sender<bool>`) is the single point that ever calls
  `active_tx.send`. Both `Inhibit`/`UnInhibit` and the peer-vanish watcher go
  through `Shared::apply(transition)` — there is exactly one send call site
  in the whole module.

## The trait boundary

Deliberately **one** trait, `NameClaimant` (one method, boxed future — same
shape as `sleep.rs`'s `Logind`/`LockerSpawner` and `idle.rs`'s
`PowerOffCommand`), because it is the only genuinely *outbound* D-Bus call
this module makes. `Inhibit`/`UnInhibit` are **inbound** — things D-Bus
peers call on *this* daemon — so there is nothing to mock a response from;
`InhibitStore` is the pure machine that plays the "no D-Bus in tests" role
for those instead, the same way `idle.rs`'s `IdlePolicy` needed no trait at
all. `decide_service_mode(claimant: &dyn NameClaimant) -> ServiceMode` is the
one function tested through the trait+fake pattern
(`unowned_name_is_claimed_and_serves`, `already_owned_name_stays_inert`,
`claim_error_stays_inert` — the last one proving a genuine bus error, not
just "someone else owns it", also resolves to the conservative `Inert`
default).

Everything else in the module — the `#[zbus::interface]` impl, the
`ObjectServer` registration, `NameOwnerChanged` signal decoding — is
plumbing, exercised live rather than unit-tested, same division `idle.rs`
draws around its Wayland thread.

## Structural enforcement of "inhibits gate idle only, never sleep"

Not just a comment: `inhibit.rs` has **no import, field, or parameter**
anywhere that reaches `sleep.rs`. `main.rs`'s `modules::inhibit::run` call
takes only `(inhibit_tx, shutdown_rx)` — no `SessionLocker`, unlike
`idle::run`, which does take one. A future edit that tried to make an
inhibit suppress the before-sleep lock would have to *add* a new dependency
edge to do it, not just flip a flag — worth Stage 7 confirming this is still
true if anything in this module changes later.

## Test inventory (14 new, `src/modules/inhibit.rs`)

`InhibitStore` (pure, 9 tests): `first_inhibit_becomes_active_and_grants_
cookie_zero`, `cookies_are_distinct_and_increasing`, `releasing_the_last_
cookie_becomes_inactive`, `releasing_one_of_several_stays_active`,
`double_uninhibit_is_not_an_error_and_does_nothing`, `uninhibit_of_a_cookie_
that_never_existed_is_not_an_error`, `peer_vanished_drops_only_that_peers_
cookies`, `last_peer_vanishing_becomes_inactive`, `vanishing_of_an_unrelated_
peer_does_nothing`.

`Shared`/watch-channel visibility (2 tests, real `tokio::sync::watch`
channel, no D-Bus): `active_transitions_are_visible_on_the_watch_channel`,
`a_second_concurrent_inhibit_does_not_resend`.

`NameClaimant`/`decide_service_mode` (fakes, 3 tests):
`unowned_name_is_claimed_and_serves`, `already_owned_name_stays_inert`,
`claim_error_stays_inert`.

## Live-check results

### Inert path (real session bus — niri owns the name)

`SAOLA_CONFIG_DIR=<empty session.kdl> RUST_LOG=info ./target/debug/
saola-session`, real session, `busctl --user status org.freedesktop.
ScreenSaver` confirmed PID 1336 = niri beforehand. Daemon log:

```
INFO saola_session::modules::inhibit: saola-session: org.freedesktop.ScreenSaver is
already owned (niri's own shim, on every machine Stage 2 checked) — staying inert
rather than fighting over it; niri's own ext-idle-notify-v1 suppression already
covers Inhibit/UnInhibit upstream of this daemon (docs/SIGNALS.md §4) name="org.freedesktop.ScreenSaver"
```

Clean SIGTERM shutdown afterward (`inhibit module stopping`, sleep inhibitor
released). No process left running (`pgrep` confirmed empty afterward).

### Active path (private bus via `dbus-run-session` — nothing owns the name)

Two separate live runs, both under `dbus-run-session -- bash <script>` (a
fresh, private session bus; the real system bus — hence real logind — is
unaffected, so `sleep.rs` still took/released a real delay inhibitor each
run, same as every prior stage's live checks; `idle.rs` was configured
disabled so it never touched Wayland).

**Run 1 — `busctl --user call ... Inhibit ss ...` (one-shot connections,
the way `busctl call` always works: connect, call, disconnect):**

```
INFO inhibit: claimed the ScreenSaver name — serving Inhibit/UnInhibit name="org.freedesktop.ScreenSaver"
INFO inhibit: ScreenSaver inhibit became active — idle actions will be suppressed until it clears
INFO inhibit: ScreenSaver Inhibit granted app="saola-live-test" reason="verifying Stage 6 active path" peer=":1.2" cookie=0 active_count=1
WARN inhibit: a ScreenSaver-inhibiting peer disappeared from the bus without calling UnInhibit — dropped its cookie(s) rather than leaving idle permanently suppressed peer=":1.2" grants="saola-live-test (verifying Stage 6 active path)" removed=1
INFO inhibit: last ScreenSaver inhibit cleared — idle actions resume arming
```

This was not the explicit-`UnInhibit` path — it accidentally, but
usefully, exercised **peer-vanish cleanup** instead: `busctl call` opens a
new connection per invocation and disconnects immediately after the reply,
so by the time the script's follow-up `busctl call ... UnInhibit u 0` ran
(as yet another new, distinct peer), the *original* inhibiting peer had
already vanished and its cookie had already been cleaned up automatically —
proof the peer-vanish path works exactly as designed, unprompted. The
follow-up `UnInhibit` call itself returned no error (the "unknown/
already-released cookie" no-op path, logged only at `debug!`, so it does not
appear at `RUST_LOG=info`).

**Run 2 — a throwaway Rust client (`scratchpad/inhibit-client`, `zbus =
"5"`, the same pinned version this crate uses) holding one persistent
connection across both calls, specifically to exercise the explicit path
Run 1 missed:**

```
client unique name: :1.1
Inhibit -> cookie 0
UnInhibit(0) sent on the same connection
```

```
INFO inhibit: claimed the ScreenSaver name — serving Inhibit/UnInhibit name="org.freedesktop.ScreenSaver"
INFO inhibit: ScreenSaver inhibit became active — idle actions will be suppressed until it clears
INFO inhibit: ScreenSaver Inhibit granted app="saola-persistent-test" reason="explicit UnInhibit path" peer=":1.1" cookie=0 active_count=1
INFO inhibit: last ScreenSaver inhibit cleared — idle actions resume arming
```

No peer-vanish `WARN` this time (the connection stayed open across both
calls and closed gracefully afterward) — confirming the explicit-release
path is a distinct code path from peer-vanish cleanup, and both work.
Between the two runs, every combination Stage 6's task list asks for
(grant, explicit release, and crash/vanish release) has now been observed
live, not just unit-tested.

Both `dbus-run-session` runs and the daemon processes were confirmed torn
down afterward (`pgrep -af "saola-session|dbus-run-session"` empty).

### Firefox — not tested against this module's own code, and here's why

Task item asks "what Firefox actually does when playing a video, if
convenient." On Jordan's real machine this is **not observable as this
module's behavior**, structurally: niri already owns
`org.freedesktop.ScreenSaver` there, so this shim stays inert and never
receives any `Inhibit` call Firefox makes — Firefox's video-inhibit-idle
behavior on this machine is 100% niri's own shim (`docs/SIGNALS.md` §4,
Stage 2), not a line of this crate's code. Running Firefox against the
private `dbus-run-session` bus instead would prove nothing meaningful either
(Firefox would need to be launched with `DBUS_SESSION_BUS_ADDRESS` pointed
at that private bus, and even then would be exercising this shim in an
environment with no real video/compositor tie-in). Given the live evidence
above already exercises the exact `Inhibit(app, reason) -> cookie` /
`UnInhibit(cookie)` surface Firefox would drive, and that a private-bus
Firefox launch would be a much larger, less certain undertaking for
marginal additional confidence, this was not attempted. Flagging explicitly
per the task's instruction not to paper over unachieved verification: **the
Firefox-specific live check was not done**; the general Inhibit/UnInhibit
surface it would exercise was, live, twice.

## Things for Stage 7 to look hardest at

1. **The empty-`peer` edge case** (`inhibit()`'s handler, `header.sender()`
   returning `None`): documented inline as "should never happen on a routed
   session-bus connection" (the bus daemon always stamps `sender`), but if
   it ever did, that cookie could only ever be released by an explicit
   `UnInhibit` — peer-vanish cleanup could never match it (an empty string
   peer never equals a real vanishing unique name). This is exactly the
   "leaked cookie is a permanent never-lock-on-idle" severity-1 shape
   Architecture's task list warns about, even though the precondition for
   it is believed unreachable given how the connection is always
   established (`zbus::Connection::session()`, never peer-to-peer). Worth a
   second opinion on whether "believed unreachable" is good enough here.
2. **The cookie-`wrapping_add` reuse scenario**, called out inline in the
   module doc comment and `InhibitStore::inhibit`'s doc comment — accepted
   as out of scope (4 billion concurrent inhibits) but flagged for a second
   look.
3. **`Shared::lock_store`'s poisoned-mutex recovery**
   (`unwrap_or_else(|poisoned| poisoned.into_inner())`): means a prior panic
   inside a critical section (which should be impossible under the no-panic
   rule, since nothing in `InhibitStore`'s methods can panic — no
   `unwrap`/`expect`/indexing that isn't already bounds-checked via
   `HashMap`) would be silently absorbed rather than surfaced. Consistent
   with "don't let one bad lock take down the D-Bus dispatcher", but worth
   Stage 7 confirming there truly is no panic path inside the guarded
   sections for this reasoning to hold.
4. **D-Bus surface / amplification**: `Inhibit` takes two caller-controlled
   strings (`application_name`, `reason_for_inhibit`) with **no length
   bound** before they're stored in `Grant` and later formatted into log
   lines. A hostile or buggy peer could call `Inhibit` with megabyte-sized
   strings repeatedly (bounded only by cookie space and D-Bus's own message
   size cap, not by anything this module enforces) — Architecture's Stage 7
   task list explicitly names "giant strings" as something to check for.
   Not mitigated in this stage; flagged here as the clearest candidate.
5. **Cookie-flood / rapid inhibit-uninhibit**: also named in Architecture's
   Stage 7 task list ("cookie floods, rapid inhibit/uninhibit"). This
   module places no upper bound on how many concurrent grants one peer (or
   the process as a whole) can hold — memory use is `O(active grants)` with
   no cap. Given the realistic threat model (this daemon's own user's own
   apps on their own session bus, not a multi-tenant service), this was
   judged acceptable for Stage 6's scope, but it is exactly the kind of
   thing Stage 7's "unauthenticated session-bus service" framing asks to be
   checked.
6. **`Shared::apply`'s `send` failure is silently ignored** (`let _ =
   self.active_tx.send(...)`) — correct per Stage 5's handoff (errors only
   when every receiver, i.e. `idle::run`, is already gone, which is already
   the fatal condition `main.rs`'s task-death handling catches), but worth
   Stage 7 double-checking that reasoning still holds given `idle::run`'s
   `inhibit_closed` latch (`idle.rs`) — if `idle::run` observed the *stub's*
   sender vanish before this module's real sender ever got a chance to send
   (a shutdown-ordering race at startup), does the `inhibit_closed` latch
   correctly re-arm for the *new* sender, or does it stay latched forever
   having only ever seen the old channel go away? (It should be fine — it's
   the same channel, same sender field, `inhibit_tx` is moved not
   recreated — but this is exactly the kind of interaction Stage 7 exists
   to double-check rather than take on faith.)
