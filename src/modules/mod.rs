//! The daemon's three concerns, each a pure state machine over injected
//! events (Architecture): `sleep` (Stage 4, the safety core — logind
//! before-sleep locking and the session `Lock` signal), `idle` (Stage 5,
//! `ext-idle-notify-v1` policy), and `inhibit` (Stage 6, the
//! `org.freedesktop.ScreenSaver` shim).
//!
//! `sleep` is the safety core. It also owns the crate's single locker-spawn
//! path ([`sleep::SessionLocker`]) — `idle`'s idle-lock timeout reuses that
//! handle rather than growing a second implementation (Architecture: "one
//! implementation, not two").
//!
//! `idle` (Stage 5) is `ext-idle-notify-v1` policy: a pure state machine
//! (`idle::IdlePolicy`) fed by a dedicated Wayland-dispatch thread, same
//! trait-and-fake shape as `sleep`.
//!
//! `inhibit` (Stage 6) is the `org.freedesktop.ScreenSaver` shim: a pure
//! cookie-bookkeeping machine (`inhibit::InhibitStore`) plus a conditional
//! D-Bus name claim, feeding `idle`'s inhibit-active `watch` channel and
//! never touching `sleep` at all (inhibits gate idle only, per Architecture).

pub mod idle;
pub mod inhibit;
pub mod sleep;
