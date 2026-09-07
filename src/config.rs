//! `~/.config/saola/session.toml` — idle timeouts, the locker command, and
//! the before-sleep-lock toggle.
//!
//! # Why TOML, and why walked by hand (teaching note)
//!
//! **2026-09-07**: this module was KDL through 0.1.0. The Saola family moved
//! its user-facing config files to TOML — `saola-capture` first (2026-08-08),
//! then `saola-greeter` and `saola-notifications`, with the panel and the
//! lockscreen migrating alongside this crate — so `session.toml` is what
//! keeps this daemon's config the same shape as every other component's. See
//! `Cargo.toml`'s dated dependency essay for the crate pick itself.
//!
//! The parse style does **not** change with the format. [`toml::Table`] is
//! walked explicitly here — `table.get("locker")`, `.as_str()`, … — instead
//! of deriving `serde::Deserialize` on [`SessionConfig`], for the same two
//! reasons the KDL version gave: a reader newer to Rust can trace an explicit
//! walk line by line (CLAUDE.md's teaching-note rule), and a hand-written
//! extractor can name exactly *which* knob was bad ("lock-before-sleep "yes"
//! is not a boolean — using default true") where a one-shot "deserialize
//! failed" cannot. `toml`'s default features do pull in `serde` internally
//! (that is how `Table`/`Value` get their own `Deserialize` impls), but that
//! is `toml` parsing into its own generic value tree, not this module
//! deriving anything.
//!
//! The file is entirely optional, every knob has a built-in default, and it
//! is read **once at startup** — no live reload (Architecture / PLAN.md
//! context: a daemon restart via `systemctl --user restart` is cheap enough
//! that live-reload isn't worth the complexity).
//!
//! # Schema
//!
//! ```toml
//! # ~/.config/saola/session.toml — every key optional; omit the file for defaults.
//! locker = "saola-lockscreen"   # default; any command to spawn
//! lock-before-sleep = true      # default
//!
//! [idle]
//! lock-after-secs = 300         # whole seconds; omit to disable the idle lock
//! power-off-after-secs = 600    # whole seconds; omit to disable output power-off
//! ```
//!
//! **Key order matters in TOML, and only in one direction:** the bare
//! top-level keys must come *before* the `[idle]` table header, because every
//! key written after a table header belongs to that table. A
//! `lock-before-sleep` written under `[idle]` is read as `idle.
//! lock-before-sleep`, which this loader never looks at — it is an unknown
//! key, silently ignored, and before-sleep locking quietly keeps its default.
//! The sample above shows the order that works; the README's copy shows the
//! same one.
//!
//! Unlike the lockscreen's `lockscreen { }` wrapper node, there is no outer
//! `[session]` table: `session.toml` is already this daemon's own file, so
//! `locker` and `lock-before-sleep` are bare top-level keys. `[idle]` is an
//! ordinary sub-table, not a namespace envelope — it exists because the two
//! idle timeouts really are one concern. Kebab-case survives the format
//! change: TOML's bare-key grammar allows `-` alongside alphanumerics and
//! `_`, so `lock-before-sleep` needs no quoting.
//!
//! # Why `-secs` in the timeout key names
//!
//! The family's implemented precedent is a bare key with the unit only in the
//! docs (`saola-capture`'s `delay`, the greeter's `failure-delay`). This
//! daemon deliberately breaks that precedent, because the old KDL
//! `lock-after` meant whole **minutes**. Reusing the bare name with a
//! silently changed unit would turn a copied-over `lock-after = 5` into a
//! five-*second* lock loop — a config that still parses, still looks right,
//! and locks the screen every five seconds. The unit in the key name makes
//! the change impossible to miss: `lock-after-secs = 5` reads as five
//! seconds because it says so.
//!
//! Seconds rather than minutes is the second half of that choice: sub-minute
//! timeouts are what the nested-niri live-testing procedure (CLAUDE.md) runs
//! on, and the old KDL schema needed a whole second value form (`"30s"`,
//! `"5m"` strings) just to express them. One integer unit removes the suffix
//! parser entirely — there is no `"90s"` form any more, in either direction.
//!
//! # Resilience rules (binding — mirrors the siblings' loaders, stricter
//! consequence)
//!
//! A daemon whose whole job is keeping the session locked must never let a
//! config typo turn that job off — Architecture's failure severity order
//! ranks "machine suspends or idles with the session unlocked/exposed" as
//! the single worst outcome, above "spurious lock" or even "suspend
//! blocked". A bad config file must never land on the unlocked side of that
//! ranking:
//!
//! - **No file at all** → [`SessionConfig::default`], silently. The
//!   expected case for anyone who hasn't written a `session.toml` yet — and
//!   note the default is `lock_before_sleep: true`, so "no config" already
//!   means "before-sleep locking is on". The one exception to the silence is
//!   a leftover `session.kdl` sitting where the TOML file would go: see
//!   [`warn_if_stale_kdl_sibling`].
//! - **File present but not valid TOML** ("garbage") → one `tracing::warn!`
//!   naming the file and the parse error, then the whole config falls back
//!   to [`SessionConfig::default`] — not a partial merge (same reasoning as
//!   every sibling loader: a document that doesn't even parse gives this
//!   module nothing safe to partially trust). Falling back to the default
//!   still means `lock_before_sleep: true` — a garbage file can silence
//!   idle-lock (which defaults to disabled anyway) but can never silence
//!   before-sleep locking.
//! - **File parses, but a single knob's value is nonsense** (a
//!   `lock-before-sleep` that isn't a bool, a `lock-after-secs` that isn't a
//!   positive integer, an empty `locker` string) → warn on that one knob,
//!   keep the rest of the document, and default just that knob. Critically,
//!   `lock-before-sleep`'s per-knob default is `true`, exactly like the
//!   whole-document default — there is no code path, bad-document or
//!   bad-knob, that resolves this field to `false` other than an explicit,
//!   well-formed `lock-before-sleep = false` in the file.
//! - **Unknown keys** → silently ignored, at the top level and inside
//!   `[idle]` alike. This loader only ever asks for the keys it knows; it
//!   never enumerates the table, so a stray key costs nothing and a future
//!   knob can be added without an older daemon complaining about it.
//!
//! Every one of these paths is unit-tested below.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use toml::Table;

/// The fixed file name every resolved config directory is joined with.
const FILE_NAME: &str = "session.toml";

/// The pre-0.1.0 file name, kept only so [`warn_if_stale_kdl_sibling`] can
/// recognize a config nobody has ported yet. Nothing ever parses it.
const STALE_FILE_NAME: &str = "session.kdl";

/// `locker`'s default value — see the module doc comment's schema section.
const DEFAULT_LOCKER: &str = "saola-lockscreen";

/// The whole of `session.toml`, resolved to typed values — loaded once at
/// boot (`SessionConfig::load`, called from `main.rs`) and never re-read
/// (module doc comment: no live reload).
///
/// Derives `Debug`/`PartialEq` for the same two reasons the siblings'
/// config structs do: `assert_eq!` in the tests below needs both, and the
/// `Debug` impl is what `main.rs`'s `--check-config` prints, which is also
/// what keeps every field here "used" as far as `cargo clippy -D
/// warnings`'s dead-code lint is concerned.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    /// The `[idle]` table — see [`IdleConfig`].
    pub idle: IdleConfig,
    /// `locker = "command"` — see the module doc comment's schema section.
    /// Never empty: an empty or absent value resolves to
    /// [`DEFAULT_LOCKER`].
    pub locker: String,
    /// `lock-before-sleep = <bool>` — see the module doc comment's schema
    /// and resilience-rules sections. Defaults `true` on every fallback
    /// path, not just the happy path.
    pub lock_before_sleep: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle: IdleConfig::default(),
            locker: DEFAULT_LOCKER.to_string(),
            lock_before_sleep: true,
        }
    }
}

/// The `[idle]` table's two independently-optional timeouts, resolved to
/// [`Duration`]s. The on-disk keys carry a `-secs` suffix (module doc
/// comment: the unit is in the name on purpose); the Rust fields do not,
/// because a [`Duration`] already carries its own unit. `None` means "this
/// action is disabled", not "use some other timeout" — there is no built-in
/// non-`None` default for either field (module doc comment: idle policy is
/// opt-in, unlike before-sleep locking, which is opt-out).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdleConfig {
    /// `idle.lock-after-secs` — spawn the locker after this long with no
    /// `ext-idle-notify-v1` activity. Stage 5 consumes this.
    pub lock_after: Option<Duration>,
    /// `idle.power-off-after-secs` — run `niri msg action
    /// power-off-monitors` after this long idle. Stage 5 consumes this,
    /// independent of `lock_after`.
    pub power_off_after: Option<Duration>,
}

/// A document that failed to parse as TOML at all — the "garbage file" case.
/// Deliberately the only error this module has: once the document parses,
/// every remaining problem (a bad knob value) is handled knob-by-knob with
/// a warning, never by returning `Err` — see the module doc comment.
#[derive(Debug)]
pub struct ConfigError(toml::de::Error);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

impl SessionConfig {
    /// Load the config at boot. Never fails — see the module doc comment's
    /// resilience rules; every error path warns (via `tracing::warn!`) and
    /// returns a value, not a `Result`. Called exactly once, from
    /// `main.rs`.
    pub fn load() -> Self {
        let Some(path) = resolve_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    fn load_from(path: &Path) -> Self {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            // Covers both "the file doesn't exist" (the common case) and
            // any other I/O error (permissions, …) — both degrade to
            // defaults, same as every sibling loader: an I/O error here is
            // not "malformed TOML", so it does not get the parse failure's
            // warning. The one thing worth a word is a leftover
            // `session.kdl`, which is why the hint lives on exactly this
            // branch and nowhere else.
            Err(_) => {
                warn_if_stale_kdl_sibling(path);
                return Self::default();
            }
        };
        match Self::parse(&contents) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "session.toml is not valid TOML — using defaults"
                );
                Self::default()
            }
        }
    }

    /// Parse a `session.toml` document's contents into a [`SessionConfig`].
    ///
    /// Returns `Err` **only** if `contents` isn't valid TOML at all — every
    /// other problem (an absent key, a bad knob value, an unknown key)
    /// resolves to that one knob's default and is reported with
    /// `tracing::warn!` rather than failing the whole parse. This is the
    /// function the unit tests below exercise directly, without touching the
    /// filesystem.
    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        // `contents.parse::<Table>()` is the whole "read the document"
        // step: it produces a generic key → value tree, and everything
        // below is an explicit walk over it. Nothing is deserialized into
        // `SessionConfig` directly — see the module doc comment.
        let body: Table = contents.parse().map_err(ConfigError)?;

        let idle_body = read_idle_table(&body);
        // `and_then` rather than `map`: the sub-table may be absent (no
        // `[idle]` written) *and* each key inside it may be absent, and
        // both mean the same thing here — that action stays disabled.
        let lock_after = idle_body.and_then(|idle| read_secs(idle, "lock-after-secs"));
        let power_off_after = idle_body.and_then(|idle| read_secs(idle, "power-off-after-secs"));

        let locker = read_locker(&body).unwrap_or_else(|| DEFAULT_LOCKER.to_string());
        // The severity-critical fallback: `unwrap_or(true)`, never
        // `unwrap_or(default.lock_before_sleep)` read from somewhere that
        // could change. Absent, wrong type, and unreadable all land here.
        let lock_before_sleep = read_bool(&body, "lock-before-sleep").unwrap_or(true);

        Ok(SessionConfig {
            idle: IdleConfig {
                lock_after,
                power_off_after,
            },
            locker,
            lock_before_sleep,
        })
    }
}

/// The migration hint: a `session.toml` that doesn't exist is unremarkable on
/// its own (nobody has to write one), but a **sibling `session.kdl`** sitting
/// exactly where the TOML file would go is almost certainly a pre-2026-09-07
/// config that nobody has ported. Worth one `tracing::warn!` naming both
/// paths so the fix is obvious, without turning it into an error — defaults
/// still apply exactly as they would for any other missing file.
///
/// Returns whether the stale file was found, which is what makes the
/// behaviour unit-testable: a bare log line is not something a test can
/// assert on without capturing the subscriber's output.
fn warn_if_stale_kdl_sibling(toml_path: &Path) -> bool {
    // `with_file_name`, not `with_extension`: the two names are fixed
    // constants, so swapping the whole file name says what is meant even if
    // the TOML path ever grows a dotted component.
    let kdl_path = toml_path.with_file_name(STALE_FILE_NAME);
    if !kdl_path.is_file() {
        return false;
    }
    tracing::warn!(
        stale = %kdl_path.display(),
        expected = %toml_path.display(),
        "found a session.kdl but no session.toml — the config format moved to TOML and \
         session.kdl is no longer read; copy its knobs into session.toml (the idle timeouts \
         are now whole seconds, named lock-after-secs / power-off-after-secs), or delete it \
         to stop seeing this hint — using defaults for now"
    );
    true
}

/// Where `session.toml` lives: the resolved config **directory** joined with
/// the fixed file name. Same three-rung chain as every sibling
/// (`SAOLA_CONFIG_DIR` / `XDG_CONFIG_HOME/saola` / `~/.config/saola`); this
/// daemon takes no command-line config-dir override, matching the
/// lockscreen (a `systemd --user` unit has no interactive terminal handing
/// it flags any more than a session locker does).
///
/// `None` only when nothing in the chain resolves (no Saola or XDG var, and
/// no `$HOME`) — treated the same as "no file": defaults.
fn resolve_path() -> Option<PathBuf> {
    config_dir_from(
        std::env::var_os("SAOLA_CONFIG_DIR"),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
    .map(|dir| dir.join(FILE_NAME))
}

/// The testable core of [`resolve_path`]'s directory chain: every
/// environment variable is a plain argument instead of read from the
/// process environment directly, so precedence can be unit-tested without
/// mutating (and thereby racing every other test in this binary against)
/// the real environment — identical helper to every sibling's.
///
/// An env var set to the **empty string** is treated as unset and falls
/// through to the next rung, matching the XDG spec's own rule for
/// `$XDG_CONFIG_HOME` applied uniformly to `$SAOLA_CONFIG_DIR` too.
fn config_dir_from(
    saola: Option<OsString>,
    xdg: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(saola) = saola
        && !saola.is_empty()
    {
        return Some(PathBuf::from(saola));
    }
    if let Some(xdg) = xdg
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("saola"));
    }
    // Same empty-means-unset rule as the two vars above — a `HOME=""`
    // would otherwise produce the *relative* path `.config/saola`.
    home.filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".config/saola"))
}

/// The `[idle]` sub-table, if it is present *and* is actually a table.
///
/// Absent is the ordinary case (idle policy is opt-in) and stays silent. A
/// key called `idle` holding something that isn't a table (`idle = 5`, the
/// shape a KDL-era muscle memory might produce) is a mistake worth exactly
/// one warning, after which it is treated as absent — both timeouts stay
/// disabled, and no per-key warning fires for keys that could not have been
/// read anyway.
fn read_idle_table(body: &Table) -> Option<&Table> {
    let value = body.get("idle")?;
    match value.as_table() {
        Some(table) => Some(table),
        None => {
            tracing::warn!("session.toml: idle {value} is not a table — ignored");
            None
        }
    }
}

/// `idle.lock-after-secs` / `idle.power-off-after-secs` as a [`Duration`].
///
/// The only accepted form is a TOML integer strictly greater than zero,
/// counted in whole seconds (module doc comment: the unit is in the key
/// name). A float, a string, zero, or a negative number all warn and fall
/// back to `None` (disabled) — the same per-knob resilience rule every other
/// bad value in this file gets. `None` here already *is* the safe default
/// (idle actions are opt-in), so this fallback never touches the severity-1
/// concern the way `lock-before-sleep`'s does.
///
/// TOML integers are `i64`, so `u64::try_from` is what rejects a negative
/// value without a cast that could wrap — the crate's no-panic rule rules
/// out `as` conversions that silently produce nonsense here.
fn read_secs(table: &Table, name: &str) -> Option<Duration> {
    let value = table.get(name)?;
    match value
        .as_integer()
        .and_then(|secs| u64::try_from(secs).ok())
        .filter(|secs| *secs > 0)
    {
        Some(secs) => Some(Duration::from_secs(secs)),
        None => {
            tracing::warn!(
                "session.toml: idle.{name} {value} is not a positive whole number of \
                 seconds — ignored"
            );
            None
        }
    }
}

/// `locker = "command"`. Present but not a string, or a string that is empty
/// after trimming, both warn and fall back to `None` (the caller's default):
/// an empty command is as unusable as no command at all, and spawning it
/// literally would fail at every lock trigger instead of once here.
fn read_locker(body: &Table) -> Option<String> {
    let value = body.get("locker")?;
    let trimmed = value.as_str().map(str::trim).filter(|s| !s.is_empty());
    match trimmed {
        Some(command) => Some(command.to_string()),
        None => {
            tracing::warn!(
                "session.toml: locker {value} is not a non-empty string — using default \
                 \"{DEFAULT_LOCKER}\""
            );
            None
        }
    }
}

/// `lock-before-sleep = <bool>` as a `bool`. A key present but holding a
/// non-boolean value warns and falls back to `None`, which the caller turns
/// into `true` — the severity-critical rule stated in the module doc
/// comment: there is no path from a malformed value to "locking off".
fn read_bool(body: &Table, name: &str) -> Option<bool> {
    let value = body.get(name)?;
    match value.as_bool() {
        Some(b) => Some(b),
        None => {
            tracing::warn!("session.toml: {name} {value} is not a boolean — using default true");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty document (what `load_from` effectively sees when a real
    /// file is missing and falls back before ever calling `parse`) yields
    /// the built-in default.
    #[test]
    fn default_config_parses() {
        let config = SessionConfig::parse("").expect("an empty document is valid TOML");
        assert_eq!(config, SessionConfig::default());
    }

    /// Every knob the schema defines, set to non-default values, all land
    /// correctly — in the key order the schema requires (bare top-level
    /// keys first, then the `[idle]` table).
    #[test]
    fn full_config_parses() {
        let toml = r#"
            locker = "swaylock"
            lock-before-sleep = false

            [idle]
            lock-after-secs = 300
            power-off-after-secs = 600
        "#;
        let config = SessionConfig::parse(toml).expect("well-formed TOML");

        assert_eq!(
            config,
            SessionConfig {
                idle: IdleConfig {
                    lock_after: Some(Duration::from_secs(300)),
                    power_off_after: Some(Duration::from_secs(600)),
                },
                locker: "swaylock".to_string(),
                lock_before_sleep: false,
            }
        );
    }

    /// Sub-minute timeouts are the point of the seconds unit — a value the
    /// old KDL schema needed a `"30s"` string form to express is now just an
    /// integer, and it is taken literally with no rounding to whole minutes.
    #[test]
    fn sub_minute_timeouts_are_taken_literally() {
        let config =
            SessionConfig::parse("[idle]\nlock-after-secs = 20").expect("well-formed TOML");
        assert_eq!(config.idle.lock_after, Some(Duration::from_secs(20)));
    }

    /// A config that only sets one knob leaves the rest at their
    /// defaults — proves knob-by-knob fallback, not "any knob present
    /// disables all defaults".
    #[test]
    fn partial_config_parses() {
        let toml = r#"
            [idle]
            lock-after-secs = 900
        "#;
        let config = SessionConfig::parse(toml).expect("well-formed TOML");

        assert_eq!(config.idle.lock_after, Some(Duration::from_secs(900)));
        assert_eq!(config.idle.power_off_after, None);
        assert_eq!(config.locker, DEFAULT_LOCKER);
        assert!(config.lock_before_sleep);
    }

    /// `lock-after-secs` and `power-off-after-secs` are independently
    /// optional — only setting one never implies or disables the other, and
    /// an empty `[idle]` table is the same as no table at all.
    #[test]
    fn idle_actions_are_independent() {
        let only_power_off = SessionConfig::parse("[idle]\npower-off-after-secs = 1200")
            .expect("well-formed TOML")
            .idle;
        assert_eq!(only_power_off.lock_after, None);
        assert_eq!(
            only_power_off.power_off_after,
            Some(Duration::from_secs(1200))
        );

        let neither = SessionConfig::parse("[idle]")
            .expect("well-formed TOML")
            .idle;
        assert_eq!(neither, IdleConfig::default());
    }

    /// A non-integer `lock-after-secs` warns and defaults just that knob —
    /// the rest of the document (here, `power-off-after-secs`) still loads.
    /// This is the single-bad-knob resilience rule, distinct from a
    /// whole-document parse failure below.
    #[test]
    fn non_integer_lock_after_is_ignored() {
        let toml = r#"
            [idle]
            lock-after-secs = "soon"
            power-off-after-secs = 600
        "#;
        let config = SessionConfig::parse(toml).expect("well-formed TOML");

        assert_eq!(config.idle.lock_after, None);
        assert_eq!(config.idle.power_off_after, Some(Duration::from_secs(600)));
    }

    /// A timeout given as a TOML float is rejected too: the schema is whole
    /// seconds, and a `0.5` that silently truncated to zero would disable
    /// the action without saying so.
    #[test]
    fn float_lock_after_is_ignored() {
        let config =
            SessionConfig::parse("[idle]\nlock-after-secs = 5.5").expect("well-formed TOML");
        assert_eq!(config.idle.lock_after, None);
    }

    /// Zero and negative second counts are nonsense for a timeout — both
    /// fall back to disabled rather than being taken literally (a zero-second
    /// idle timeout would fire continuously).
    #[test]
    fn non_positive_secs_are_ignored() {
        let zero = SessionConfig::parse("[idle]\nlock-after-secs = 0").expect("well-formed TOML");
        assert_eq!(zero.idle.lock_after, None);

        let negative =
            SessionConfig::parse("[idle]\npower-off-after-secs = -5").expect("well-formed TOML");
        assert_eq!(negative.idle.power_off_after, None);
    }

    /// A key called `idle` that isn't a table at all (`idle = 5`) warns once
    /// and is treated as absent — both timeouts stay disabled and the rest of
    /// the document still loads.
    #[test]
    fn idle_that_is_not_a_table_is_ignored() {
        let toml = r#"
            idle = 5
            locker = "swaylock"
        "#;
        let config = SessionConfig::parse(toml).expect("well-formed TOML");

        assert_eq!(config.idle, IdleConfig::default());
        assert_eq!(config.locker, "swaylock");
    }

    /// The severity-critical case: a `lock-before-sleep` value that isn't a
    /// bool must fall back to `true` (locking stays on), never `false` and
    /// never propagate as an error. `"yes"` is the likely real typo — TOML
    /// booleans are bare `true`/`false` only.
    #[test]
    fn non_bool_lock_before_sleep_defaults_true() {
        for bad in [r#"lock-before-sleep = "yes""#, "lock-before-sleep = 0"] {
            let config = SessionConfig::parse(bad).expect("well-formed TOML");
            assert!(
                config.lock_before_sleep,
                "{bad} must not disable before-sleep locking"
            );
        }
    }

    /// An explicit, well-formed `false` is the *only* way this field
    /// resolves to `false` — proven alongside the above so the two cases are
    /// never confused.
    #[test]
    fn explicit_false_lock_before_sleep_is_honored() {
        let config = SessionConfig::parse("lock-before-sleep = false").expect("well-formed TOML");
        assert!(!config.lock_before_sleep);
    }

    /// An empty (or all-whitespace) `locker` is as unusable as no command at
    /// all — falls back to the default rather than being spawned literally.
    #[test]
    fn empty_locker_falls_back_to_default() {
        for empty in [r#"locker = """#, r#"locker = "   ""#] {
            let config = SessionConfig::parse(empty).expect("well-formed TOML");
            assert_eq!(config.locker, DEFAULT_LOCKER);
        }
    }

    /// A `locker` given as a non-string value also falls back cleanly.
    #[test]
    fn non_string_locker_falls_back_to_default() {
        let config = SessionConfig::parse("locker = 42").expect("well-formed TOML");
        assert_eq!(config.locker, DEFAULT_LOCKER);
    }

    /// Unknown keys are never enumerated, so they cost nothing: one at the
    /// top level and one inside `[idle]`, and everything the loader does know
    /// about still lands.
    #[test]
    fn unknown_keys_are_ignored() {
        let toml = r#"
            locker = "swaylock"
            enable-teleportation = true

            [idle]
            lock-after-secs = 300
            dim-after-secs = 120
        "#;
        let config = SessionConfig::parse(toml).expect("well-formed TOML");

        assert_eq!(
            config,
            SessionConfig {
                idle: IdleConfig {
                    lock_after: Some(Duration::from_secs(300)),
                    power_off_after: None,
                },
                locker: "swaylock".to_string(),
                lock_before_sleep: true,
            }
        );
    }

    /// Syntactically invalid TOML is the one case `parse` itself rejects —
    /// `load_from` is what turns this `Err` into a full-default fallback plus
    /// a warning.
    #[test]
    fn garbage_is_rejected_by_parse() {
        let result = SessionConfig::parse("this is not = valid [[[ toml");
        assert!(result.is_err());
    }

    /// `load_from`'s fallback path, exercised directly against a temp file
    /// so the "malformed file → full defaults, including
    /// `lock_before_sleep: true`" resilience rule is proven end to end, not
    /// just at the `parse` layer.
    #[test]
    fn garbage_file_falls_back_to_defaults() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "saola-session-test-garbage-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "this is not = valid [[[ toml").expect("temp dir is writable");

        let config = SessionConfig::load_from(&path);

        std::fs::remove_file(&path).ok();
        assert_eq!(config, SessionConfig::default());
        assert!(
            config.lock_before_sleep,
            "a garbage file must not disable before-sleep locking"
        );
    }

    /// The missing-file default path: a path that doesn't exist at all falls
    /// back to defaults, not an error and not a panic.
    #[test]
    fn missing_file_falls_back_to_defaults() {
        let path = std::env::temp_dir().join("saola-session-test-definitely-missing.toml");
        std::fs::remove_file(&path).ok();

        let config = SessionConfig::load_from(&path);

        assert_eq!(config, SessionConfig::default());
    }

    /// A leftover `session.kdl` next to a missing `session.toml` is detected
    /// and hinted at — and defaults still apply exactly as they would for any
    /// other missing file. The KDL file is never parsed.
    #[test]
    fn a_stale_kdl_sibling_is_reported() {
        let dir = std::env::temp_dir().join(format!(
            "saola-session-test-stale-kdl-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir is writable");
        let toml_path = dir.join(FILE_NAME);
        let kdl_path = dir.join(STALE_FILE_NAME);
        std::fs::write(&kdl_path, "idle { lock-after 5 }").expect("temp dir is writable");

        let found = warn_if_stale_kdl_sibling(&toml_path);
        let config = SessionConfig::load_from(&toml_path);

        std::fs::remove_dir_all(&dir).ok();
        assert!(found, "the sibling session.kdl must be detected");
        assert_eq!(config, SessionConfig::default());
    }

    /// No sibling, no hint — the ordinary "nobody wrote a config" case must
    /// stay silent.
    #[test]
    fn no_stale_kdl_sibling_is_not_reported() {
        let dir = std::env::temp_dir().join(format!(
            "saola-session-test-no-stale-kdl-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir is writable");
        let toml_path = dir.join(FILE_NAME);

        let found = warn_if_stale_kdl_sibling(&toml_path);

        std::fs::remove_dir_all(&dir).ok();
        assert!(!found);
    }

    /// `$SAOLA_CONFIG_DIR` wins over both `$XDG_CONFIG_HOME` and `$HOME`.
    #[test]
    fn saola_env_wins_over_xdg_and_home() {
        let dir = config_dir_from(
            Some("/saola".into()),
            Some("/xdg".into()),
            Some("/home/jordan".into()),
        );
        assert_eq!(dir, Some(PathBuf::from("/saola")));
    }

    /// `$XDG_CONFIG_HOME/saola` wins over `$HOME` when `$SAOLA_CONFIG_DIR`
    /// is unset.
    #[test]
    fn xdg_wins_over_home() {
        let dir = config_dir_from(None, Some("/xdg".into()), Some("/home/jordan".into()));
        assert_eq!(dir, Some(PathBuf::from("/xdg/saola")));
    }

    /// `~/.config/saola` is the last resort when neither env var is set.
    #[test]
    fn home_is_the_last_resort() {
        let dir = config_dir_from(None, None, Some("/home/jordan".into()));
        assert_eq!(dir, Some(PathBuf::from("/home/jordan/.config/saola")));
    }

    /// An env var set to the empty string is treated as unset, not as a
    /// literal empty path — the same rule the XDG spec states for
    /// `$XDG_CONFIG_HOME` and this loader applies uniformly to
    /// `$SAOLA_CONFIG_DIR` too.
    #[test]
    fn empty_env_var_is_treated_as_unset() {
        let dir = config_dir_from(
            Some("".into()),
            Some("".into()),
            Some("/home/jordan".into()),
        );
        assert_eq!(dir, Some(PathBuf::from("/home/jordan/.config/saola")));
    }

    /// Nothing set anywhere in the chain resolves to `None` — the "no
    /// config is possible here" case, not an error.
    #[test]
    fn nothing_set_resolves_to_none() {
        let dir = config_dir_from(None, None, None);
        assert_eq!(dir, None);
    }
}
