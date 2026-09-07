//! saola-session — idle, sleep and lock wiring for the Saola desktop
//! environment.
//!
//! Stage 1 was a stub that printed the crate version and exited. Stage 3
//! (this file) adds the real pieces around that: a small synchronous CLI
//! surface (`--version`, `--check-config`), config loading, logging setup,
//! and the tokio event loop skeleton that Stage 4/5/6 plug their modules
//! into. See `PLAN.md`'s Architecture section for the full design and
//! `CLAUDE.md` for the binding rules (no panics on any runtime path, the
//! severity order, the sudo rule) this file and everything downstream of it
//! must hold to.

mod config;
mod modules;

use std::process::ExitCode;

use config::SessionConfig;

/// The daemon's whole command-line surface. Deliberately hand-rolled rather
/// than pulling in a flag-parsing crate (`clap` and friends) — two
/// exit-and-print flags plus "no flags at all" doesn't earn a dependency,
/// and Stage 1's `Cargo.toml` survey didn't budget for one. Parsed from
/// `std::env::args()` directly in [`main`], never from inside the async
/// runtime — see [`main`]'s doc comment for why that ordering matters.
#[derive(Debug, PartialEq, Eq)]
enum Cli {
    /// No recognized flag: run the daemon for real.
    Run,
    /// `--version` (or `-V`): print the crate version and exit. No config
    /// load, no logging setup, no runtime — as cheap as this binary gets.
    Version,
    /// `--check-config`: load `session.toml` exactly as the real daemon
    /// would, print the resolved [`SessionConfig`], and exit. Stage 8's
    /// README points operators at this for validating a config edit
    /// without restarting the `systemd --user` unit.
    CheckConfig,
    /// Anything else: not a crash, just a usage error on stderr and a
    /// non-zero exit — a typo'd flag must never silently fall through to
    /// `Run` and start the daemon with defaults the operator didn't ask
    /// for.
    Unrecognized(String),
}

impl Cli {
    /// Parses the arguments *after* the program name (`main` passes
    /// `std::env::args().skip(1)`). Only the first argument is inspected —
    /// this daemon has exactly two flags and neither takes a value, so
    /// there is nothing further to parse.
    fn parse<I: Iterator<Item = String>>(mut args: I) -> Self {
        match args.next() {
            None => Cli::Run,
            Some(arg) if arg == "--version" || arg == "-V" => Cli::Version,
            Some(arg) if arg == "--check-config" => Cli::CheckConfig,
            Some(arg) => Cli::Unrecognized(arg),
        }
    }
}

/// Teaching note: why `main` is synchronous and builds its own
/// [`tokio::runtime::Runtime`] instead of `#[tokio::main]`.
///
/// `#[tokio::main]` is sugar for exactly the `Builder::new_multi_thread()
/// ... .block_on(async_main())` call in [`run`] below — but it stands up
/// the runtime *before* a single line of `main`'s body runs. That's the
/// wrong shape for this binary: `--version` and `--check-config` are meant
/// to be the cheapest possible paths (Stage 3's own instructions call
/// `--check-config` "cheap"), and neither one does any async work at all.
/// Building a multi-thread runtime just to print a string and exit would
/// be pure overhead — worse, a runtime that fails to construct (starved
/// file descriptors, a broken `/proc`, whatever) would turn `--version`
/// into a hard failure for a daemon that hasn't even tried to do anything
/// yet. So `main` stays plain, decides which of the three paths it's on
/// from argv alone, and only [`run`] — the "actually be the daemon" path —
/// ever touches tokio.
fn main() -> ExitCode {
    match Cli::parse(std::env::args().skip(1)) {
        Cli::Run => run(),
        Cli::Version => {
            println!("saola-session {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Cli::CheckConfig => {
            // Stage 7's finding L-1: `SessionConfig::load` reports every
            // per-knob problem (an unparseable document, a bad `locker`,
            // a nonsense timeout) via `tracing::warn!` — but until this
            // fix, only `run()` ever installed a subscriber, so every one
            // of those warnings was emitted into a void and this command
            // printed a silently-defaulted config with no indication
            // anything in the file was ignored. `--check-config` is a
            // human-facing command; default to `warn` rather than `info`
            // (still overridable via `RUST_LOG`) so the output is the
            // parsed config plus exactly its complaints, nothing louder.
            init_tracing("warn");

            // Same loader the real daemon uses (`SessionConfig::load`), so
            // this prints the *actually effective* config — including
            // every per-knob fallback `config.rs`'s resilience rules would
            // apply — not a re-implementation that could drift from it.
            let config = SessionConfig::load();
            println!("{config:#?}");
            ExitCode::SUCCESS
        }
        Cli::Unrecognized(arg) => {
            eprintln!("saola-session: unrecognized argument '{arg}'");
            eprintln!("usage: saola-session [--version | --check-config]");
            ExitCode::FAILURE
        }
    }
}

/// The real daemon path: logging, config, the tokio runtime, and the event
/// loop. No `unwrap`/`expect`/`panic!` below this point (`CLAUDE.md`'s
/// no-panic rule) — every fallible step here degrades to a logged error and
/// a non-zero [`ExitCode`] instead, because the failure mode this whole
/// crate exists to avoid is "looks running, does nothing" (`CLAUDE.md`),
/// and a startup panic would at least be loud and get `Restart=on-failure`
/// (Stage 8) — but there's no reason to reach for panic when a plain
/// `Result` says the same thing without depending on unwind behavior.
fn run() -> ExitCode {
    init_tracing("info");

    let config = SessionConfig::load();
    tracing::info!(?config, "saola-session: effective configuration loaded");

    // Teaching note: building the runtime by hand (vs. `#[tokio::main]`)
    // is explained on `main`'s doc comment above; `enable_all()` turns on
    // both the timer driver (`IdleConfig`'s idle timeouts, Stage 4's
    // `LockPending` deadline) and the I/O driver (needed by `tokio::
    // process`, `tokio::signal`, and zbus's own tokio integration —
    // everything Stage 4/5/6 add).
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(error = %err, "saola-session: failed to start the tokio runtime");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async_main(config))
}

/// The event loop itself. Teaching note on the shape below, since this is
/// the pattern every later stage's module plugs into:
///
/// - **Shutdown fan-out**: a [`tokio::sync::watch`] channel carries a
///   single `bool` ("please stop") to every module task. `watch` (rather
///   than, say, a `oneshot` per task) is the right primitive because its
///   receiver is `Clone` and remembers the *last* value sent — a task that
///   subscribes late (or is still mid-spawn when shutdown fires) still
///   sees `true` immediately instead of missing a one-shot message that
///   already fired. Each module task owns its own clone of the receiver;
///   nothing is shared through a `Mutex`.
/// - **Task ownership**: each module task is spawned once and owns
///   whatever external resource it holds for its entire lifetime —
///   `modules::sleep::run` owns the logind delay-inhibitor file descriptor
///   exactly this way. That's why shutdown is cooperative (a watch signal
///   the task notices and acts on) rather than the runtime just being
///   dropped: a task that owns a resource needing explicit release (the
///   inhibitor fd today; the D-Bus name Stage 6's shim will hold) needs the
///   chance to run its own `Drop`/release logic in an orderly fashion, not
///   have the process ripped out from under it.
/// - **Joining before exit**: after signaling shutdown, this function
///   awaits every task via [`tokio::task::JoinSet`] before returning. By
///   the time `async_main` returns (and `run` returns the process's exit
///   code), every module's cleanup has actually run, not just been
///   requested. A bare `std::process::exit` after firing the signal would
///   race that cleanup against process teardown — which for `sleep.rs`
///   means racing the release of the inhibitor fd against the process going
///   away, i.e. logind seeing a delay lock that outlives the daemon.
/// - **A task that returns on its own is fatal** — see the `select!` below.
async fn async_main(config: SessionConfig) -> ExitCode {
    tracing::info!("saola-session: starting");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Stage 4: the sleep module's external wiring (the logind connection,
    // its signal subscriptions, the locker spawner) is built here rather
    // than inside the task, for one reason — `SleepWiring::session_locker`
    // is the handle Stage 5's idle module must reuse for its own
    // spawn-if-not-locked path, and it has to be taken before `wiring` is
    // moved into the task. `connect` never fails: with no logind reachable
    // it logs and returns a degraded wiring (the nested-niri case
    // `CLAUDE.md` makes binding).
    let wiring = modules::sleep::connect(&config).await;
    let session_locker = wiring.session_locker();

    // Stage 5's inhibit-active seam: a `watch` channel, same shape as the
    // shutdown one above. `idle::run` holds the receiver and treats "no
    // sender has ever sent" (the initial `false`) as "no inhibit" — exactly
    // the default-to-no-inhibit behavior Stage 6 must preserve. `inhibit_tx`
    // is handed to Stage 6's module seam below so *something* keeps the
    // sender alive until shutdown (see `idle::run`'s doc comment for what
    // happens if it were dropped early: the receiver stops erroring in a
    // busy loop, but real inhibit updates obviously stop arriving).
    let (inhibit_tx, inhibit_rx) = tokio::sync::watch::channel(false);

    // Stage 6: the real ScreenSaver inhibit shim, replacing the Stage 5 stub.
    // It owns `inhibit_tx` for its whole task lifetime (same reason the stub
    // did — see `idle::run`'s doc comment on `inhibit_closed`) and never
    // touches `session_locker`/`sleep`: inhibits gate idle only, never the
    // before-sleep lock (Architecture, binding), enforced here simply by
    // this module never being handed a `SessionLocker` to begin with.
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    tasks.spawn(modules::sleep::run(wiring, shutdown_rx.clone()));
    tasks.spawn(modules::idle::run(
        config.idle,
        session_locker,
        inhibit_rx,
        shutdown_rx.clone(),
    ));
    tasks.spawn(modules::inhibit::run(inhibit_tx, shutdown_rx.clone()));

    // Teaching note (Stage 4): a module task returning *before* shutdown was
    // requested is a fatal condition, not a quiet one. `CLAUDE.md`'s
    // no-panic rule is explicit that a half-alive daemon — one that looks
    // running but has lost, say, its logind connection and will never lock
    // before sleep again — is worse than a crash, because systemd's
    // `Restart=on-failure` (Stage 8) can only rescue a process that
    // actually dies. So whichever of these two arms completes first decides
    // the exit code: an operator-requested stop is a success, a task
    // returning on its own is a failure that gets us restarted.
    let exit_code = tokio::select! {
        // Stage 7's finding L-6: `biased` so a genuine shutdown always wins
        // a simultaneous wakeup, matching every module's own `select!`. At
        // logout, systemd's SIGTERM and niri exiting (which kills the
        // Wayland socket, ending `idle::run`) can both become ready in the
        // same poll; without `biased` the coin flip that lost would log a
        // false-positive "a module task stopped on its own" `ERROR` and
        // exit 1 on every ordinary logout — exactly the journal noise
        // Stage 8's README tells Jordan to trust.
        biased;

        shutdown_signalled = wait_for_shutdown_signal() => {
            if shutdown_signalled {
                tracing::info!("saola-session: shutdown signal received, stopping module tasks");
                ExitCode::SUCCESS
            } else {
                // Stage 7's finding L-2: `wait_for_shutdown_signal` also
                // returns if it could not install its signal handlers in
                // the first place (fd exhaustion, or similar) — a real
                // failure, not an operator-requested stop. Treating that as
                // `ExitCode::SUCCESS` (the old behavior) meant
                // `Restart=on-failure` would never fire for a daemon that
                // can no longer hear SIGTERM at all: it would just exit 0
                // and stay down. Distinguishing the two cases here is what
                // makes this genuinely a failure systemd restarts from.
                tracing::error!(
                    "saola-session: could not wait for a shutdown signal — stopping so systemd \
                     restarts us into a daemon that can actually hear SIGTERM/SIGINT"
                );
                ExitCode::FAILURE
            }
        }
        _ = tasks.join_next() => {
            tracing::error!(
                "saola-session: a module task stopped on its own — exiting non-zero so the \
                 daemon is restarted rather than left running without one of its concerns"
            );
            ExitCode::FAILURE
        }
    };

    // `send` only errors when every receiver has already been dropped —
    // i.e. every module task already exited on its own, which is exactly
    // the fatal case handled above. Either way there is nothing left to
    // signal, so the error is intentionally ignored rather than logged as
    // if it were surprising.
    let _ = shutdown_tx.send(true);

    while let Some(result) = tasks.join_next().await {
        if let Err(err) = result {
            // A `JoinError` here means a module task panicked (it should
            // not have — `CLAUDE.md`'s no-panic rule — but `JoinSet`
            // catches the unwind rather than taking the process down by
            // itself) or was cancelled. Either way: log loudly rather than
            // let it pass silently, per the severity rule that a strand-
            // but-look-alive daemon is worse than one that visibly failed.
            tracing::error!(error = %err, "saola-session: a module task did not shut down cleanly");
        }
    }

    tracing::info!("saola-session: stopped");
    exit_code
}

/// Waits for `SIGTERM` or `SIGINT` — the two ways systemd (`systemctl
/// --user stop`, unit teardown) and an interactive `cargo run` (Ctrl-C)
/// ask this daemon to stop. Returns `true` once either arrives.
///
/// Returns `false` (with an error logged) if the signal handlers themselves
/// can't be installed, since spinning forever pretending shutdown can still
/// happen cleanly would be worse than falling through to whatever the
/// caller does next — and, per Stage 7's finding L-2, the caller must be
/// able to tell that case apart from a real signal: the two used to be
/// indistinguishable (`()` either way), which meant `async_main` reported a
/// broken signal path as `ExitCode::SUCCESS`, and `Restart=on-failure`
/// never got the chance to fix it.
async fn wait_for_shutdown_signal() -> bool {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(sigterm) => sigterm,
        Err(err) => {
            tracing::error!(error = %err, "saola-session: failed to install a SIGTERM handler");
            return false;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(sigint) => sigint,
        Err(err) => {
            tracing::error!(error = %err, "saola-session: failed to install a SIGINT handler");
            return false;
        }
    };

    tokio::select! {
        _ = sigterm.recv() => tracing::info!("saola-session: received SIGTERM"),
        _ = sigint.recv() => tracing::info!("saola-session: received SIGINT"),
    }
    true
}

/// Sets up `tracing-subscriber`'s `fmt` layer on stderr (which `systemd`
/// already journals per-unit — see `CLAUDE.md`'s logging rationale) with
/// `RUST_LOG`-driven verbosity via `env-filter`, defaulting to
/// `default_level` when `RUST_LOG` is unset or invalid. Called exactly once
/// per process — from [`run`] (the real daemon path, `"info"`) or from
/// `main`'s `CheckConfig` arm (`"warn"` — Stage 7's finding L-1) — before
/// which point nothing in this crate has any severity-bearing events to
/// emit yet, and the two call sites are mutually exclusive branches of the
/// same `match`, so this never runs twice in one process.
fn init_tracing(default_level: &str) {
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_means_run() {
        assert_eq!(Cli::parse(std::iter::empty()), Cli::Run);
    }

    #[test]
    fn version_flag_is_recognized() {
        assert_eq!(
            Cli::parse(std::iter::once("--version".to_string())),
            Cli::Version
        );
        assert_eq!(Cli::parse(std::iter::once("-V".to_string())), Cli::Version);
    }

    #[test]
    fn check_config_flag_is_recognized() {
        assert_eq!(
            Cli::parse(std::iter::once("--check-config".to_string())),
            Cli::CheckConfig
        );
    }

    #[test]
    fn unknown_flag_is_reported_not_silently_run() {
        assert_eq!(
            Cli::parse(std::iter::once("--bogus".to_string())),
            Cli::Unrecognized("--bogus".to_string())
        );
    }
}
