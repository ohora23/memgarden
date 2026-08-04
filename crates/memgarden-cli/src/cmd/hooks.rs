//! `memgarden hooks install | uninstall | status` — HK-2, the cutover switch.
//!
//! # This is the only subcommand family that is not a hook
//!
//! Everything else in this binary is spawned by Claude Code thousands of times
//! per session and must never exit 2 (`lib.rs`). These three are typed by a
//! human at a shell, once. Two consequences, both deliberate:
//!
//! * `install` **may exit 1**, and does, when `--mode full` meets a legacy
//!   installation. A refusal that exits 0 is a refusal a script cannot see.
//!   The never-2 guarantee is untouched: `dispatch` returns `ExitCode` and the
//!   only two values it can produce are `SUCCESS` and `FAILURE`.
//! * `MEMGARDEN_HOOKS_DISABLE` does **not** gate them. A tool that reports
//!   whether the hooks are wired has to work precisely when they are turned
//!   off; that is the state the user is asking about.
//!
//! # Why installing cannot turn anything on
//!
//! Wiring and enabling are two independent layers (plan §Coexistence), and
//! this command owns only the first:
//!
//! * **wiring** — the four entries in `settings.json`, written here;
//! * **runtime mode** — `[hooks] mode` in `config.toml`, read by `hook recall`
//!   on every prompt, and `[hooks] enabled` / `MEMGARDEN_HOOKS_DISABLE`.
//!
//! So `--mode` here is a **declaration of intent**, not a setting: it selects
//! the double-injection gate below, and it is checked against the mode the
//! hooks will actually run in. It deliberately does not write `config.toml` —
//! that file is TOML with the user's own comments in it, and a second
//! comment-preserving splice engine for a value the user can set with one line
//! of `$EDITOR` is exactly the complexity this phase refuses. `install`
//! defaults to `shadow`, and even `--mode full` leaves the runtime in whatever
//! `config.toml` says, printing the one line that changes it.
//!
//! That is what makes "the switch must not flip anything by existing" true by
//! construction rather than by discipline (plan §Binding decisions #13).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use memgarden_core::config::Config;
use memgarden_core::now_ms;

use crate::settings::{self, SettingsError};
use crate::state;

/// Hand-parsed argv, like everything in this binary — `clap`'s usage errors
/// exit 2, and a binary that can produce a 2 for a typo is a binary that can
/// erase a prompt (`lib.rs`).
struct Args {
    mode: String,
    settings: Option<PathBuf>,
    dry_run: bool,
    allow_double_injection: bool,
    clear_poison: Option<String>,
    unknown: Vec<String>,
}

fn parse(argv: &[String]) -> Args {
    let mut args = Args {
        // Shadow by default and nowhere else: the default lives here, in the
        // parse, so no caller can construct an install that defaults to full.
        mode: "shadow".to_string(),
        settings: None,
        dry_run: false,
        allow_double_injection: false,
        clear_poison: None,
        unknown: Vec::new(),
    };
    let mut it = argv.iter().skip(2);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => args.mode = it.next().cloned().unwrap_or_default(),
            "--settings" => args.settings = it.next().map(PathBuf::from),
            "--clear-poison" => args.clear_poison = it.next().cloned(),
            "--dry-run" => args.dry_run = true,
            "--allow-double-injection" => args.allow_double_injection = true,
            other => args.unknown.push(other.to_string()),
        }
    }
    args
}

pub fn run(sub: &str, argv: &[String]) -> ExitCode {
    let args = parse(argv);
    if !args.unknown.is_empty() {
        println!("unrecognised argument(s): {}", args.unknown.join(" "));
        println!("usage: memgarden hooks install|uninstall|status [--settings <path>] [--dry-run]");
        return ExitCode::FAILURE;
    }
    match sub {
        "install" => install(&args),
        "uninstall" => uninstall(&args),
        "status" => status(&args),
        other => {
            println!("unknown subcommand: hooks {other}");
            println!("usage: memgarden hooks install|uninstall|status");
            ExitCode::FAILURE
        }
    }
}

/// `~/.claude/settings.json`, or `$CLAUDE_CONFIG_DIR/settings.json` when the
/// user has moved their Claude Code config.
///
/// `--settings <path>` overrides both, and **every test uses it**: plan
/// §Cross-PR rules 1 forbids a test ever writing the real file. `--dry-run` is
/// the only thing allowed to point at it, and only by hand.
fn settings_path(args: &Args) -> Option<PathBuf> {
    if let Some(p) = &args.settings {
        return Some(p.clone());
    }
    let dir = match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var("HOME").ok()?).join(".claude"),
    };
    Some(dir.join("settings.json"))
}

/// The absolute path that goes into `command`.
///
/// `current_exe` rather than `argv[0]`: the entry has to keep working when the
/// user's `PATH` does not include the binary, which is the normal case for a
/// hook spawned by a GUI-launched Claude Code. Canonicalised so a
/// `~/.cargo/bin` symlink resolves once, here, rather than on every hook
/// invocation for the life of the installation.
fn binary_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn install(args: &Args) -> ExitCode {
    let Some(path) = settings_path(args) else {
        println!("cannot resolve settings.json: neither CLAUDE_CONFIG_DIR nor HOME is set");
        return ExitCode::FAILURE;
    };
    let Some(bin) = binary_path() else {
        println!("cannot resolve this binary's own path");
        return ExitCode::FAILURE;
    };
    if !matches!(args.mode.as_str(), "shadow" | "full") {
        println!("--mode must be shadow or full, got {:?}", args.mode);
        return ExitCode::FAILURE;
    }

    // A missing settings.json is created from `{}` rather than being an error:
    // a fresh Claude Code install has none, and refusing there would send the
    // user to hand-write JSON before they could use the switch.
    let existed = path.exists();
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) if !existed => "{}\n".to_string(),
        Err(e) => {
            println!("cannot read {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };

    let doc: serde_json::Value = match serde_json::from_str(&src) {
        Ok(d) => d,
        Err(e) => {
            println!("{}", SettingsError::Parse(e.to_string()));
            return ExitCode::FAILURE;
        }
    };

    // The double-injection gate. Ours strips `<hindsight_memories>` before
    // ingesting (CE-5b), so legacy's injections never re-enter our bank —
    // but legacy's strip list has never heard of `<memgarden_memories>`, so in
    // `full` mode it would retain our block into its own. We cannot fix
    // legacy, so we refuse instead.
    let legacy = legacy_wiring(&doc);
    if args.mode == "full" && !legacy.is_empty() && !args.allow_double_injection {
        println!("refusing --mode full: the legacy hindsight hooks are still wired.");
        for (event, cmd) in &legacy {
            println!("  {event}: {}", elide(cmd, 100));
        }
        println!();
        println!("In full mode MemGarden prints <memgarden_memories> and legacy retains it");
        println!("into its own bank — legacy's strip list does not know our tag. Either");
        println!("remove those entries, install with --mode shadow (the supported way to");
        println!("run both), or pass --allow-double-injection if you meant it.");
        return ExitCode::FAILURE;
    }

    let splice = match settings::install(&src, &bin) {
        Ok(s) => s,
        Err(e) => {
            println!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if splice.is_noop() {
        println!(
            "already installed: all four events are wired in {}",
            path.display()
        );
        report_runtime(args);
        return ExitCode::SUCCESS;
    }

    println!("--- {}", path.display());
    for line in &splice.changed {
        println!("+{line}");
    }
    if args.dry_run {
        println!();
        println!("--dry-run: nothing written.");
        return ExitCode::SUCCESS;
    }

    if existed {
        match settings::backup(&path, &state_dir(), now_ms()) {
            Ok(dest) => println!("backup: {}", dest.display()),
            Err(e) => {
                // No backup, no write. The backup is the recovery path for the
                // one race the atomic write cannot close, so proceeding
                // without it would be proceeding without the escape hatch.
                println!("cannot write a backup ({e}) — nothing written");
                return ExitCode::FAILURE;
            }
        }
    }
    // What the file holds *on disk*, which is not `src` when there is no file:
    // a created settings.json starts from `{}` in memory and from nothing at
    // all on disk, and the concurrent-modification check compares bytes.
    let on_disk: &[u8] = if existed { src.as_bytes() } else { b"" };
    if let Err(e) = settings::write_atomic(&path, &splice.text, on_disk) {
        println!("{e}");
        return ExitCode::FAILURE;
    }

    println!(
        "installed {} entries in {}",
        splice.changed.len(),
        path.display()
    );
    println!();
    println!("This takes effect IMMEDIATELY in every running Claude Code instance —");
    println!("the settings file watcher picks the edit up mid-session, no restart.");
    report_runtime(args);
    ExitCode::SUCCESS
}

/// What the hooks will actually do now that they are wired, which is not what
/// `--mode` said.
fn report_runtime(args: &Args) {
    let cfg = Config::load().ok();
    let config_path = memgarden_core::paths::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unresolved>".into());
    match &cfg {
        Some(cfg) if !cfg.hooks.enabled => {
            println!("runtime: [hooks] enabled = false in {config_path} — every hook exits early.");
        }
        Some(cfg) => {
            println!("runtime: mode = {} (from {config_path})", cfg.hooks.mode);
            if args.mode == "full" && cfg.hooks.mode != "full" {
                println!();
                println!("--mode full does NOT change this. The runtime mode is config-owned so");
                println!("that installing the switch cannot throw it. To inject for real:");
                println!("  [hooks] mode = \"full\"   in {config_path}");
            }
        }
        None => println!("runtime: cannot read {config_path} — defaults apply (mode = shadow)"),
    }
}

fn uninstall(args: &Args) -> ExitCode {
    let Some(path) = settings_path(args) else {
        println!("cannot resolve settings.json: neither CLAUDE_CONFIG_DIR nor HOME is set");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            println!("cannot read {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let splice = match settings::uninstall(&src) {
        Ok(s) => s,
        Err(e) => {
            println!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if splice.is_noop() {
        println!(
            "nothing to remove: no memgarden entries in {}",
            path.display()
        );
        return ExitCode::SUCCESS;
    }

    println!("--- {}", path.display());
    for line in &splice.changed {
        println!("-{line}");
    }
    if args.dry_run {
        println!();
        println!("--dry-run: nothing written.");
        return ExitCode::SUCCESS;
    }
    match settings::backup(&path, &state_dir(), now_ms()) {
        Ok(dest) => println!("backup: {}", dest.display()),
        Err(e) => {
            println!("cannot write a backup ({e}) — nothing written");
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = settings::write_atomic(&path, &splice.text, src.as_bytes()) {
        println!("{e}");
        return ExitCode::FAILURE;
    }
    println!(
        "removed {} entries from {}",
        splice.changed.len(),
        path.display()
    );
    println!();
    println!("Your session state and the SQLite bank are untouched. Re-installing");
    println!("resumes from the recorded offsets; nothing is lost or double-ingested.");
    ExitCode::SUCCESS
}

/// The diagnostic. **Always exits 0** — a status command that fails a script
/// on a degraded daemon is a status command people stop running.
fn status(args: &Args) -> ExitCode {
    if let Some(sid) = &args.clear_poison {
        clear_poison(sid);
        println!();
    }

    let cfg = Config::load();
    match memgarden_core::paths::config_path() {
        Ok(p) => println!("config      {}", p.display()),
        Err(e) => println!("config      unresolved ({e})"),
    }
    match &cfg {
        Ok(cfg) => {
            let disabled_by_env = crate::hooks_disabled(
                std::env::var_os(memgarden_core::config::ENV_HOOKS_DISABLE).as_deref(),
            );
            println!(
                "hooks       enabled = {}{}",
                cfg.hooks.enabled,
                if disabled_by_env {
                    "  (overridden: MEMGARDEN_HOOKS_DISABLE is set — every hook is a no-op)"
                } else {
                    ""
                }
            );
            println!("mode        {}", cfg.hooks.mode);
            println!("daemon url  {}", cfg.hooks.daemon_url);
            println!("state dir   {}", cfg.hooks.state_dir.display());
        }
        Err(e) => println!("config      unreadable ({e}) — showing what does not need it"),
    }
    println!();

    // --- wiring, per event ---
    let path = settings_path(args);
    match &path {
        Some(p) => println!("settings    {}", p.display()),
        None => println!("settings    unresolved (neither CLAUDE_CONFIG_DIR nor HOME is set)"),
    }
    let doc = path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let (mut ours, mut theirs) = (Vec::new(), Vec::new());
    match &doc {
        Some(doc) => {
            ours = settings::wired_events(doc);
            theirs = legacy_wiring(doc);
            for entry in settings::ENTRIES {
                let mine = if ours.contains(&entry.event) {
                    "memgarden"
                } else {
                    "-"
                };
                let legacy = if theirs.iter().any(|(e, _)| e == entry.event) {
                    "hindsight"
                } else {
                    "-"
                };
                println!("  {:<17} {:<10} {legacy}", entry.event, mine);
            }
        }
        None => println!("  (unreadable or not valid JSON — nothing else can be said about it)"),
    }
    println!();

    // --- the daemons ---
    println!("memgardend  {}", probe_daemon(cfg.as_ref().ok()));
    println!(
        "hindsight   {}",
        if listening(9077) {
            "listening on 127.0.0.1:9077"
        } else {
            "not listening on 127.0.0.1:9077"
        }
    );
    println!();

    // --- local state ---
    if let Ok(cfg) = &cfg {
        let sessions = state::load_all(&cfg.hooks.state_dir);
        let poisoned: Vec<&crate::state::SessionState> = sessions
            .iter()
            .filter(|s| s.poisoned_at.is_some())
            .collect();
        println!("sessions    {} state files", sessions.len());
        if let Some(oldest) = oldest_state_file(&cfg.hooks.state_dir) {
            println!("oldest      {oldest}");
        }
        if !poisoned.is_empty() {
            println!(
                "poisoned    {} — clear with --clear-poison <session-id>",
                poisoned.len()
            );
            for s in poisoned {
                println!("  {} bank={} offset={}", s.session_id, s.bank_id, s.offset);
            }
        }
        unconfirmed(&cfg.hooks, &sessions);
    }

    // --- the warnings that only matter while both are wired ---
    if !ours.is_empty() && !theirs.is_empty() {
        println!();
        println!("BOTH SYSTEMS ARE WIRED.");
        println!("  * GPU contention: both retain pipelines extract with Ollama on this box,");
        println!("    and legacy already holds qwen3-14b-nothink. Watch the retain_jobs");
        println!("    backlog in memdash; chunk failures get likely under contention.");
        if cfg.as_ref().is_ok_and(|c| c.hooks.mode == "full") {
            println!("  * DOUBLE INJECTION: mode = full prints <memgarden_memories>, and");
            println!("    legacy's strip list does not know that tag — it will retain our");
            println!("    block into its own bank. Shadow is the supported way to run both.");
        }
    }
    ExitCode::SUCCESS
}

/// How many bytes this machine has POSTed and not seen confirmed.
///
/// **This exists because the shadow run cannot otherwise evaluate its own
/// re-entry criterion.** The open daemon defect — a `done` job with
/// `chunks_failed > 0` opens a cursor gap, and the worker's unconditional
/// `confirmed_offset` write then erases the evidence via the `MAX` merge —
/// is mitigated today by a `debug`-gated stderr line, and `debug` defaults to
/// false. A default installation would therefore show nothing at all about the
/// one number the cutover gate is supposed to read
/// (`docs/design/c4b-hook-retain.md` §Known limits).
///
/// **It is a LOWER BOUND, and the output says so.** `inflight_bytes` is
/// `byte_offset - confirmed_offset` on the daemon's row, and the same defect
/// that opens the gap also shrinks this number. A non-zero value is real; a
/// zero is not proof of convergence.
///
/// Reading the optimistic cursor is fine **here** and nowhere else: `Mirror`
/// omits `byte_offset` by construction because seeding a *cursor* from it
/// skips the bytes the dual cursor exists to protect. Nothing in this function
/// writes state — it prints a number.
///
/// // ponytail: one GET per session, capped at 10 most-recent, and the cap is
/// // printed rather than silent. A `GET …/sessions` list route already exists
/// // (C1) if that ever needs to be one request.
fn unconfirmed(cfg: &memgarden_core::config::HooksConfig, sessions: &[crate::state::SessionState]) {
    const MAX_PROBES: usize = 10;

    // Locally-known pending jobs first: no daemon needed, and it is the
    // clearest statement of "this hook posted bytes nobody has confirmed".
    let pending: Vec<&crate::state::SessionState> =
        sessions.iter().filter(|s| s.pending.is_some()).collect();
    for s in &pending {
        if let Some(p) = &s.pending {
            println!(
                "  {} pending job={} bytes={} (posted, unconfirmed)",
                s.session_id,
                p.job_id,
                p.offset_to.saturating_sub(p.offset_from)
            );
        }
    }

    let Ok(target) = super::target(cfg) else {
        return;
    };
    let timeouts = super::interactive_timeouts(cfg);
    let probed = sessions.len().min(MAX_PROBES);
    let mut total = 0i64;
    let mut seen = 0usize;
    for s in sessions.iter().take(MAX_PROBES) {
        let path = format!(
            "/v1/banks/{}/sessions/{}",
            crate::http::encode_path_segment(&s.bank_id),
            crate::http::encode_path_segment(&s.session_id)
        );
        let Ok(response) = crate::http::get(&target, &path, &timeouts) else {
            // The daemon being down is already reported four lines up; a
            // second complaint per session would bury the rest of the report.
            return;
        };
        if !response.is_success() {
            continue;
        }
        let Ok(row) = serde_json::from_slice::<MirrorStatus>(&response.body) else {
            continue;
        };
        seen += 1;
        let inflight = row.byte_offset.saturating_sub(row.confirmed_offset).max(0);
        total += inflight;
        if inflight > 0 {
            println!(
                "  {} inflight={} B (byte_offset {} - confirmed {})",
                s.session_id, inflight, row.byte_offset, row.confirmed_offset
            );
        }
    }
    if seen > 0 {
        println!(
            "unconfirmed {total} B across {seen} of {} sessions{} — a LOWER BOUND while the \
             chunks_failed cursor gap is open (docs/design/c4b-hook-retain.md)",
            sessions.len(),
            if sessions.len() > probed {
                format!(", capped at {MAX_PROBES}")
            } else {
                String::new()
            }
        );
    }
}

/// The daemon's session row, read **only** to report a number.
///
/// Deliberately not [`super::Mirror`]: that struct omits `byte_offset` so a
/// cursor can never be seeded from the optimistic value, and that property is
/// worth keeping exactly as it is. This is the diagnostic's own view, in the
/// one place where the *difference* between the two cursors is the answer.
#[derive(Debug, serde::Deserialize)]
struct MirrorStatus {
    #[serde(default)]
    byte_offset: i64,
    #[serde(default)]
    confirmed_offset: i64,
}

fn clear_poison(sid: &str) {
    let Ok(cfg) = Config::load() else {
        println!("--clear-poison: cannot read the config");
        return;
    };
    let dir = cfg.hooks.state_dir.as_path();
    let done = state::with_lock(dir, sid, || {
        let Some(mut st) = state::load(dir, sid) else {
            return false;
        };
        st.poisoned_at = None;
        // The reject counter is what poisons the session in the first place;
        // clearing the stamp without it means the next durable 4xx re-poisons
        // immediately and the operator's intervention lasted one request.
        st.reject_failures = 0;
        state::store(dir, &st).is_ok()
    });
    match done {
        Ok(true) => println!("--clear-poison: {sid} cleared"),
        Ok(false) => println!("--clear-poison: no state file for {sid}"),
        Err(e) => println!("--clear-poison: {e}"),
    }
}

fn probe_daemon(cfg: Option<&Config>) -> String {
    let Some(cfg) = cfg else {
        return "unknown (no config)".to_string();
    };
    let target = match crate::cmd::target(&cfg.hooks) {
        Ok(t) => t,
        Err(e) => return format!("cannot build a request: {e}"),
    };
    let timeouts = crate::cmd::interactive_timeouts(&cfg.hooks);
    match crate::http::get(&target, "/livez", &timeouts) {
        Err(e) => format!(
            "down ({e})\n            start it with:  memgardend  (it is a user service, never started by a hook)"
        ),
        Ok(_) => match crate::http::get(&target, "/healthz", &timeouts) {
            Ok(r) => format!(
                "up — /healthz {} {}",
                r.status,
                elide(&String::from_utf8_lossy(&r.body), 160)
            ),
            Err(e) => format!("up — /livez ok, /healthz failed ({e})"),
        },
    }
}

/// Whether anything is accepting on loopback `port`.
///
/// A connect, not a request: we do not speak legacy's protocol and have no
/// business sending it anything. Plan §Cross-PR rules 1 — legacy is
/// untouchable, and that includes not poking its endpoints.
fn listening(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(50),
    )
    .is_ok()
}

fn oldest_state_file(dir: &Path) -> Option<String> {
    let mut oldest: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if oldest.as_ref().is_none_or(|(t, _)| modified < *t) {
            oldest = Some((modified, name));
        }
    }
    let (time, name) = oldest?;
    let age = std::time::SystemTime::now()
        .duration_since(time)
        .map(|d| format!("{} days ago", d.as_secs() / 86_400))
        .unwrap_or_else(|_| "in the future".to_string());
    Some(format!("{name} ({age})"))
}

fn legacy_wiring(doc: &serde_json::Value) -> Vec<(String, String)> {
    settings::ENTRIES
        .iter()
        .flat_map(|entry| {
            settings::legacy_commands(doc, entry.event)
                .into_iter()
                .map(|cmd| (entry.event.to_string(), cmd))
        })
        .collect()
}

fn state_dir() -> PathBuf {
    Config::load()
        .map(|c| c.hooks.state_dir)
        .or_else(|_| memgarden_core::paths::hooks_state_dir())
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Truncated on a **char** boundary, not a byte one. The commands being
/// elided are user-supplied and the live file's SessionStart hook is a Korean
/// sentence; `&s[..n]` panics on it (the same class as C3's `&s[..800]`).
fn elide(s: &str, max_chars: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max_chars {
        return s;
    }
    s.chars().take(max_chars).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(rest: &[&str]) -> Vec<String> {
        let mut v = vec!["hooks".to_string(), "install".to_string()];
        v.extend(rest.iter().map(|s| s.to_string()));
        v
    }

    /// Shadow is the default and there is exactly one place that decides it.
    #[test]
    fn the_default_mode_is_shadow() {
        assert_eq!(parse(&argv(&[])).mode, "shadow");
        assert_eq!(parse(&argv(&["--mode", "full"])).mode, "full");
        // A `--mode` with nothing after it is not "full by accident".
        assert_eq!(parse(&argv(&["--mode"])).mode, "");
    }

    #[test]
    fn flags_parse_without_clap() {
        let a = parse(&argv(&[
            "--settings",
            "/tmp/s.json",
            "--dry-run",
            "--allow-double-injection",
        ]));
        assert_eq!(a.settings, Some(PathBuf::from("/tmp/s.json")));
        assert!(a.dry_run);
        assert!(a.allow_double_injection);
        assert!(a.unknown.is_empty());
        // An unknown flag is reported, not silently ignored: this is the one
        // command family where a typo must not be a no-op that looks like a
        // success.
        assert_eq!(parse(&argv(&["--dry-runn"])).unknown, vec!["--dry-runn"]);
    }

    #[test]
    fn elide_cuts_on_a_char_boundary() {
        let korean = "재개 세션 감지 이 세션은 과거 대화를 재개한 것입니다";
        let out = elide(korean, 5);
        assert_eq!(out.chars().count(), 6, "5 chars plus the ellipsis");
        assert!(out.starts_with("재개 세"));
        assert_eq!(elide("short", 100), "short");
        assert_eq!(elide("a\nb", 100), "a b");
    }
}
