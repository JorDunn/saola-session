//! `~/.config/saola/session.kdl` — idle timeouts, the locker command, and
//! the before-sleep-lock toggle.
//!
//! Same KDL family and resolution order as the panel's `panel.kdl` and the
//! lockscreen's `lockscreen.kdl` (hand-walked KDL document via the `kdl`
//! crate, no serde derive — see either sibling's `config.rs` doc comment for
//! why: precise per-knob warnings on bad values, explicit code over a
//! derived deserializer). The file is entirely optional, every knob has a
//! built-in default, and it is read **once at startup** — no live reload
//! (Architecture / PLAN.md context: a daemon restart via `systemctl --user
//! restart` is cheap enough that live-reload isn't worth the complexity).
//!
//! # Schema
//!
//! ```kdl
//! idle {
//!     lock-after 5            // bare integer = minutes; omit to disable idle-lock
//!     power-off-after "90s"   // or a quoted duration: "30s", "5m"; omit to disable
//! }
//! locker "saola-lockscreen"   // default; any command to spawn
//! lock-before-sleep #true     // default
//! ```
//!
//! Timeouts take two forms: a bare KDL integer (whole minutes — the
//! original schema, kept so existing configs and the common case stay
//! terse) or a quoted string with an explicit unit suffix, `"30s"`
//! (seconds) or `"5m"` (minutes), added so sub-minute timeouts are
//! expressible — mainly for testing against a nested compositor, where
//! waiting out a full real minute per run is the difference between a
//! usable live test and an unusable one.
//!
//! `#true`/`#false`, not bare `true`/`false`: `Cargo.toml` pins plain
//! `kdl = "6.7.1"` (no `v1`/`v1-fallback` feature, matching both siblings'
//! pin exactly), and that crate's default parser is **KDL v2**, whose
//! spec reserves bare `true`/`false`/`null` as ordinary identifiers and
//! requires the `#`-prefixed keyword form for the boolean/null literals
//! (verified directly against this crate's own test fixtures under
//! `kdl-6.7.1/src/document.rs`, e.g. `mouse_mode #false`, not
//! `mouse_mode false` — a bare `lock-before-sleep false` fails to parse
//! with "Expected identifier string" rather than reading as a bool). This
//! is a KDL-the-format detail, not a `saola-session`-specific rule — worth
//! calling out here because neither sibling's config has had a bool knob
//! yet to hit it first.
//!
//! Unlike the lockscreen's `lockscreen { }` wrapper node, `idle { }`,
//! `locker`, and `lock-before-sleep` are all top-level nodes in this
//! document — there is no outer `session { }` envelope (Architecture's
//! sketch of this schema has none, and nothing else needs the namespacing a
//! wrapper node would buy).
//!
//! Every knob is independently optional. An empty file, a file that omits a
//! node entirely, and a file that sets every knob to its default all parse
//! to the exact same [`SessionConfig::default`]:
//!
//!   - `idle.lock-after <timeout>` — idle-lock timeout (bare minutes or a
//!     suffixed string, per the schema section above). Absent by default,
//!     which means idle-lock is **disabled**, not "some fallback timeout" —
//!     Stage 5's idle module skips registering the notification entirely
//!     when this is `None` (Architecture: "skip entirely when config
//!     disables both — don't hold a Wayland connection for nothing").
//!   - `idle.power-off-after <timeout>` — output power-off timeout. Same
//!     value forms, same absent-means-disabled rule, independent of
//!     `lock-after`.
//!   - `locker "command"` — the command Stage 4/5 spawn to lock the
//!     session. Defaults to `"saola-lockscreen"` (resolved via `$PATH` at
//!     spawn time, same as any bare command). The daemon does not parse or
//!     validate this beyond "non-empty string" — Architecture: "the daemon
//!     must not hardcode more knowledge of the locker than 'a command to
//!     spawn'".
//!   - `lock-before-sleep <bool>` — whether Stage 4's before-sleep module
//!     spawns the locker at all. Defaults to `true`. This is the one knob
//!     where "bad value" and "explicitly disabled" must never be
//!     confusable: a malformed value here falls back to `true` (locking
//!     stays on), never to `false` — see the resilience rules below.
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
//!   expected case for anyone who hasn't written a `session.kdl` yet — and
//!   note the default is `lock_before_sleep: true`, so "no config" already
//!   means "before-sleep locking is on".
//! - **File present but not valid KDL** ("garbage") → one `tracing::warn!`
//!   naming the file and the parse error, then the whole config falls back
//!   to [`SessionConfig::default`] — not a partial merge (same reasoning as
//!   both siblings' loaders: a document that doesn't even parse gives this
//!   module nothing safe to partially trust). Falling back to the default
//!   still means `lock_before_sleep: true` — a garbage file can silence
//!   idle-lock (which defaults to disabled anyway) but can never silence
//!   before-sleep locking.
//! - **File parses, but a single knob's value is nonsense** (a
//!   `lock-before-sleep` that isn't a bool, a `lock-after` that isn't a
//!   positive integer, an empty `locker` string) → warn on that one knob,
//!   keep the rest of the document, and default just that knob. Critically,
//!   `lock-before-sleep`'s per-knob default is `true`, exactly like the
//!   whole-document default — there is no code path, bad-document or
//!   bad-knob, that resolves this field to `false` other than an explicit,
//!   well-formed `lock-before-sleep #false` in the file.
//!
//! Every one of these paths is unit-tested below.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use kdl::KdlDocument;

/// The fixed file name every resolved config directory is joined with.
const FILE_NAME: &str = "session.kdl";

/// `locker`'s default value — see the module doc comment's schema section.
const DEFAULT_LOCKER: &str = "saola-lockscreen";

/// The whole of `session.kdl`, resolved to typed values — loaded once at
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
    /// The `idle { }` node — see [`IdleConfig`].
    pub idle: IdleConfig,
    /// `locker "command"` — see the module doc comment's schema section.
    /// Never empty: an empty or absent value resolves to
    /// [`DEFAULT_LOCKER`].
    pub locker: String,
    /// `lock-before-sleep <bool>` — see the module doc comment's schema and
    /// resilience-rules sections. Defaults `true` on every fallback path,
    /// not just the happy path.
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

/// The `idle { }` node's two independently-optional timeouts, resolved to
/// [`Duration`]s (the KDL surface accepts bare minutes or a suffixed
/// string — see the module doc comment's schema section; by the time a
/// value lands here the unit question is already settled). `None` means
/// "this action is disabled", not "use some other timeout" — there is no
/// built-in non-`None` default for either field (module doc comment: idle
/// policy is opt-in, unlike before-sleep locking, which is opt-out).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdleConfig {
    /// `idle.lock-after <timeout>` — spawn the locker after this long with
    /// no `ext-idle-notify-v1` activity. Stage 5 consumes this.
    pub lock_after: Option<Duration>,
    /// `idle.power-off-after <timeout>` — run `niri msg action
    /// power-off-monitors` after this long idle. Stage 5 consumes this,
    /// independent of `lock_after`.
    pub power_off_after: Option<Duration>,
}

/// A KDL document that failed to parse at all — the "garbage file" case.
/// Deliberately the only error this module has: once the document parses,
/// every remaining problem (a bad knob value) is handled knob-by-knob with
/// a warning, never by returning `Err` — see the module doc comment.
#[derive(Debug)]
pub struct ConfigError(kdl::KdlError);

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

    fn load_from(path: &std::path::Path) -> Self {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            // Covers both "the file doesn't exist" (the common case) and
            // any other I/O error (permissions, …) — both degrade to
            // defaults silently, same as the siblings' loaders: an I/O
            // error here is not "malformed KDL", so it does not get the
            // parse failure's warning.
            Err(_) => return Self::default(),
        };
        match Self::parse(&contents) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "session.kdl is not valid KDL — using defaults"
                );
                Self::default()
            }
        }
    }

    /// Parse a `session.kdl` document's contents into a [`SessionConfig`].
    ///
    /// Returns `Err` **only** if `contents` isn't valid KDL at all — every
    /// other problem (an absent node, a bad knob value) resolves to that
    /// one knob's default and is reported with `tracing::warn!` rather than
    /// failing the whole parse. This is the function the unit tests below
    /// exercise directly, without touching the filesystem.
    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let document = KdlDocument::parse(contents).map_err(ConfigError)?;

        let idle_body = document.get("idle").and_then(|node| node.children());
        let lock_after = read_arg_timeout(idle_body, "lock-after");
        let power_off_after = read_arg_timeout(idle_body, "power-off-after");

        let locker = read_arg_str(Some(&document), "locker")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                // Distinguish "absent" from "present but wrong type or
                // empty" only in the warning text — both resolve to the
                // same default, per the module doc comment's per-knob
                // resilience rule.
                if document.get_arg("locker").is_some() {
                    tracing::warn!(
                        "session.kdl: locker is not a non-empty string — using default \"{DEFAULT_LOCKER}\""
                    );
                }
                DEFAULT_LOCKER.to_string()
            });

        let lock_before_sleep = match document.get_arg("lock-before-sleep") {
            None => true,
            Some(value) => match value.as_bool() {
                Some(b) => b,
                None => {
                    tracing::warn!(
                        "session.kdl: lock-before-sleep \"{value}\" is not a bool — using default true"
                    );
                    true
                }
            },
        };

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

/// Where `session.kdl` lives: the resolved config **directory** joined with
/// the fixed file name. Same three-rung chain as both siblings
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
/// the real environment — identical helper to both siblings'.
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

/// `body.get_arg(name)` as a string, if the node exists and its first
/// positional argument is a KDL string. A node present but holding a
/// non-string value falls through to `None` — same "absent knob" fallback
/// path as the siblings' identical helper, no separate error needed for
/// "wrong value type" versus "missing entirely".
fn read_arg_str<'a>(body: Option<&'a KdlDocument>, name: &str) -> Option<&'a str> {
    body?.get_arg(name)?.as_string()
}

/// `idle.lock-after`/`idle.power-off-after` as a [`Duration`]. Two accepted
/// forms (module doc comment's schema section): a KDL integer strictly
/// greater than zero, read as whole minutes; or a KDL string with an
/// explicit unit suffix (`"30s"`, `"5m"`), which is what makes sub-minute
/// timeouts expressible. Everything else — a float, zero, a negative
/// number, a suffix-less or unknown-suffix string — warns and falls back to
/// `None` (disabled), the same per-knob resilience rule every other bad
/// value in this file gets. `None` here already *is* the safe default (idle
/// actions are opt-in, per the module doc comment), so this fallback never
/// touches the severity-1 concern the way `lock-before-sleep`'s does.
fn read_arg_timeout(body: Option<&KdlDocument>, name: &str) -> Option<Duration> {
    let value = body?.get_arg(name)?;

    // Bare integer: whole minutes, the original schema, unchanged.
    if let Some(n) = value.as_integer() {
        if let Some(n) = u32::try_from(n).ok().filter(|n| *n > 0) {
            return Some(Duration::from_secs(u64::from(n) * 60));
        }
    } else if let Some(s) = value.as_string() {
        // Quoted string: explicit unit required — accepting a bare "90"
        // would leave its unit ambiguous against the integer form's
        // minutes, so it is deliberately rejected rather than guessed at.
        if let Some(duration) = parse_suffixed_duration(s) {
            return Some(duration);
        }
    }

    tracing::warn!(
        "session.kdl: idle.{name} \"{value}\" is not a positive whole number of minutes \
         or a duration string like \"30s\"/\"5m\" — ignored"
    );
    None
}

/// `"30s"` / `"5m"` → a positive [`Duration`]; anything else → `None`.
/// The digits must parse as a positive integer — `"0s"`, `"1.5m"`, `"s"`,
/// and `"5h"` all fall through to [`read_arg_timeout`]'s warning.
/// `checked_mul` on the seconds count keeps an absurd value (`"99999999m"`
/// levels of absurd would still fit; this is `u64::MAX`-adjacent input)
/// saturating into `None` rather than panicking — the crate no-panic rule.
fn parse_suffixed_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (digits, unit_seconds) = if let Some(digits) = s.strip_suffix('s') {
        (digits, 1u64)
    } else {
        // No `s` suffix: `m` is the only other unit, and `?` bails on
        // anything else (clippy's `question_mark` prefers this shape over
        // a third `else` arm returning `None` explicitly).
        (s.strip_suffix('m')?, 60u64)
    };
    let count: u64 = digits.parse().ok().filter(|n| *n > 0)?;
    count.checked_mul(unit_seconds).map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty document (what `load_from` effectively sees when a real
    /// file is missing and falls back before ever calling `parse`) yields
    /// the built-in default.
    #[test]
    fn default_config_parses() {
        let config = SessionConfig::parse("").expect("an empty document is valid KDL");
        assert_eq!(config, SessionConfig::default());
    }

    /// Every knob the schema defines, set to non-default values, all land
    /// correctly.
    #[test]
    fn full_config_parses() {
        let kdl = r##"
            idle {
                lock-after 5
                power-off-after 10
            }
            locker "swaylock"
            lock-before-sleep #false
        "##;
        let config = SessionConfig::parse(kdl).expect("well-formed KDL");

        assert_eq!(
            config,
            SessionConfig {
                idle: IdleConfig {
                    lock_after: Some(Duration::from_secs(5 * 60)),
                    power_off_after: Some(Duration::from_secs(10 * 60)),
                },
                locker: "swaylock".to_string(),
                lock_before_sleep: false,
            }
        );
    }

    /// The quoted-string timeout form: an explicit `s`/`m` suffix, the
    /// schema's way of expressing sub-minute values (the bare-integer form
    /// is whole minutes only).
    #[test]
    fn suffixed_duration_strings_parse() {
        let kdl = r##"
            idle {
                lock-after "30s"
                power-off-after "5m"
            }
        "##;
        let config = SessionConfig::parse(kdl).expect("well-formed KDL");

        assert_eq!(config.idle.lock_after, Some(Duration::from_secs(30)));
        assert_eq!(config.idle.power_off_after, Some(Duration::from_secs(300)));
    }

    /// Seconds counts above a minute are fine too — `"90s"` means ninety
    /// seconds, no normalization or "should have written 1m30s" pedantry.
    #[test]
    fn oversized_seconds_are_taken_literally() {
        let config = SessionConfig::parse(r#"idle { lock-after "90s" }"#).expect("well-formed KDL");
        assert_eq!(config.idle.lock_after, Some(Duration::from_secs(90)));
    }

    /// Bad duration strings — zero, a missing or unknown suffix, a bare
    /// number in quotes (unit would be ambiguous), fractions — all fall
    /// back to disabled, same rule as every other bad knob value.
    #[test]
    fn bad_duration_strings_are_ignored() {
        for bad in [
            r#""0s""#,
            r#""90""#,
            r#""5h""#,
            r#""s""#,
            r#""1.5m""#,
            r#""-30s""#,
        ] {
            let kdl = format!("idle {{ lock-after {bad} }}");
            let config = SessionConfig::parse(&kdl).expect("well-formed KDL");
            assert_eq!(
                config.idle.lock_after, None,
                "{bad} should have been rejected"
            );
        }
    }

    /// A config that only sets one knob leaves the rest at their
    /// defaults — proves knob-by-knob fallback, not "any knob present
    /// disables all defaults".
    #[test]
    fn partial_config_parses() {
        let kdl = r##"
            idle {
                lock-after 15
            }
        "##;
        let config = SessionConfig::parse(kdl).expect("well-formed KDL");

        assert_eq!(config.idle.lock_after, Some(Duration::from_secs(15 * 60)));
        assert_eq!(config.idle.power_off_after, None);
        assert_eq!(config.locker, DEFAULT_LOCKER);
        assert!(config.lock_before_sleep);
    }

    /// `lock-after` and `power-off-after` are independently optional — only
    /// setting one never implies or disables the other.
    #[test]
    fn idle_actions_are_independent() {
        let only_power_off = SessionConfig::parse("idle { power-off-after 20 }")
            .expect("well-formed KDL")
            .idle;
        assert_eq!(only_power_off.lock_after, None);
        assert_eq!(
            only_power_off.power_off_after,
            Some(Duration::from_secs(20 * 60))
        );

        let neither = SessionConfig::parse("idle { }")
            .expect("well-formed KDL")
            .idle;
        assert_eq!(neither, IdleConfig::default());
    }

    /// A non-numeric `lock-after` warns and defaults just that knob — the
    /// rest of the document (here, `power-off-after`) still loads. This is
    /// the single-bad-knob resilience rule, distinct from a
    /// whole-document parse failure below.
    #[test]
    fn non_numeric_lock_after_is_ignored() {
        let kdl = r##"
            idle {
                lock-after "soon"
                power-off-after 10
            }
        "##;
        let config = SessionConfig::parse(kdl).expect("well-formed KDL");

        assert_eq!(config.idle.lock_after, None);
        assert_eq!(
            config.idle.power_off_after,
            Some(Duration::from_secs(10 * 60))
        );
    }

    /// Zero and negative minute counts are nonsense for a timeout — both
    /// fall back to disabled rather than being taken literally.
    #[test]
    fn non_positive_lock_after_is_ignored() {
        let zero = SessionConfig::parse("idle { lock-after 0 }").expect("well-formed KDL");
        assert_eq!(zero.idle.lock_after, None);

        let negative = SessionConfig::parse("idle { lock-after -5 }").expect("well-formed KDL");
        assert_eq!(negative.idle.lock_after, None);
    }

    /// A `lock-after` given as a float (not a KDL integer) is also
    /// rejected — the bare-number form is whole minutes only; sub-minute
    /// wants the quoted `"30s"` form, not `0.5`.
    #[test]
    fn float_lock_after_is_ignored() {
        let config = SessionConfig::parse("idle { lock-after 5.5 }").expect("well-formed KDL");
        assert_eq!(config.idle.lock_after, None);
    }

    /// The severity-critical case: a `lock-before-sleep` value that isn't a
    /// bool must fall back to `true` (locking stays on), never `false` and
    /// never propagate as an error.
    #[test]
    fn non_bool_lock_before_sleep_defaults_true() {
        let config = SessionConfig::parse(r#"lock-before-sleep "nope""#).expect("well-formed KDL");
        assert!(config.lock_before_sleep);
    }

    /// An explicit, well-formed `#false` is the *only* way this field
    /// resolves to `false` — proven alongside the above so the two cases
    /// are never confused. (KDL v2 keyword form — see the module doc
    /// comment's schema section for why bare `false` doesn't parse as a
    /// bool at all.)
    #[test]
    fn explicit_false_lock_before_sleep_is_honored() {
        let config = SessionConfig::parse("lock-before-sleep #false").expect("well-formed KDL");
        assert!(!config.lock_before_sleep);
    }

    /// An empty `locker ""` is as unusable as no command at all — falls
    /// back to the default rather than being spawned literally.
    #[test]
    fn empty_locker_falls_back_to_default() {
        let config = SessionConfig::parse(r#"locker "" "#).expect("well-formed KDL");
        assert_eq!(config.locker, DEFAULT_LOCKER);
    }

    /// A `locker` given as a non-string value also falls back cleanly.
    #[test]
    fn non_string_locker_falls_back_to_default() {
        let config = SessionConfig::parse("locker 42").expect("well-formed KDL");
        assert_eq!(config.locker, DEFAULT_LOCKER);
    }

    /// Syntactically invalid KDL is the one case `parse` itself rejects —
    /// `load_from` (not exercised here, since it touches the filesystem)
    /// is what turns this `Err` into a full-default fallback plus a
    /// warning.
    #[test]
    fn garbage_is_rejected_by_parse() {
        let result = SessionConfig::parse("idle { this is not } valid kdl {{{");
        assert!(result.is_err());
    }

    /// The single most likely real-world typo this schema invites: writing
    /// bare `false` (valid in plenty of other config languages, and even
    /// valid *KDL v1*) instead of the KDL v2 keyword `#false`. This is not
    /// a "wrong type for this knob" case — it makes the *entire document*
    /// fail to parse (see the module doc comment's schema section), so the
    /// whole file falls back to [`SessionConfig::default`], and critically
    /// `lock_before_sleep` lands on its safe default `true`, not on the
    /// `false` the author almost certainly intended but mistyped. This is
    /// exactly the "nonsense-values path ... must not mean 'no locking'"
    /// case Stage 3's instructions call out.
    #[test]
    fn bareword_bool_typo_fails_whole_document_and_defaults_to_locking_on() {
        let result = SessionConfig::parse("lock-before-sleep false");
        assert!(
            result.is_err(),
            "bare `false` is a KDL v2 syntax error, not a valid-but-wrong-type bool"
        );

        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "saola-session-test-bareword-bool-{}.kdl",
            std::process::id()
        ));
        std::fs::write(&path, "lock-before-sleep false").expect("temp dir is writable");

        let config = SessionConfig::load_from(&path);

        std::fs::remove_file(&path).ok();
        assert!(
            config.lock_before_sleep,
            "a config typo must never silently disable before-sleep locking"
        );
    }

    /// `load_from`'s fallback path, exercised directly against a temp file
    /// so the "malformed file → full defaults, including
    /// `lock_before_sleep: true`" resilience rule is proven end to end,
    /// not just at the `parse` layer — this is the nonsense-values-path
    /// test Stage 3's own instructions call out explicitly.
    #[test]
    fn garbage_file_falls_back_to_defaults() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "saola-session-test-garbage-{}.kdl",
            std::process::id()
        ));
        std::fs::write(&path, "idle { this is not } valid kdl {{{").expect("temp dir is writable");

        let config = SessionConfig::load_from(&path);

        std::fs::remove_file(&path).ok();
        assert_eq!(config, SessionConfig::default());
        assert!(
            config.lock_before_sleep,
            "a garbage file must not disable before-sleep locking"
        );
    }

    /// The missing-file default path (Stage 3's own instruction to cover
    /// this explicitly): a path that doesn't exist at all falls back to
    /// defaults, not an error and not a panic.
    #[test]
    fn missing_file_falls_back_to_defaults() {
        let path = std::env::temp_dir().join("saola-session-test-definitely-missing.kdl");
        std::fs::remove_file(&path).ok();

        let config = SessionConfig::load_from(&path);

        assert_eq!(config, SessionConfig::default());
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
