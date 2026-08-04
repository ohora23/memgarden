//! `memgarden hooks install | uninstall | status` against the real binary.
//!
//! **Nothing here reads or writes the user's real `~/.claude/settings.json`**
//! (plan §Cross-PR rules 1): every run passes `--settings <tempfile>`, and
//! `HOME` is redirected into a temp dir on top of that, so a bug in path
//! resolution cannot reach the real file either. `--dry-run` against the real
//! file is a manual verification step in the PR body, never a test.
//!
//! The unit tests in `src/settings.rs` cover the splice itself. This file
//! covers the parts only a process can show: the exit codes, the refusals, and
//! that `--dry-run` leaves the file's bytes alone.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The daemon's identity token. Every reply carries it, because a hook that
/// cannot tell `memgardend` apart from an impostor refuses to read the
/// response at all (C3).
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A stub `memgardend` that answers `/livez`, `/healthz` and the session
/// mirror GET. Bound to port **0** — 9077 (legacy) and 9090 (memdash) are live
/// on this machine and are never touched.
fn stub(byte_offset: i64, confirmed_offset: i64) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { continue };
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).into_owned();
            let body = if head.contains("/sessions/") {
                serde_json::json!({
                    "bank_id": "claude-code::demo",
                    "session_id": "s-inflight",
                    "chunk_index": 2,
                    "byte_offset": byte_offset,
                    "confirmed_offset": confirmed_offset,
                    "inflight_bytes": byte_offset - confirmed_offset,
                })
                .to_string()
            } else {
                "{}".to_string()
            };
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 x-memgarden-token: {TOKEN}\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(reply.as_bytes());
            let _ = sock.flush();
        }
    });
    url
}

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    settings: PathBuf,
}

/// A settings.json with legacy wired on all four events, the user's other
/// top-level keys in their real (unsorted) order, and an Orca entry sharing
/// one of the event arrays.
const WITH_LEGACY: &str = r#"{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "CLAUDE_PLUGIN_ROOT=/r/hindsight/... python3 /r/hindsight/scripts/recall.py",
            "timeout": 45,
            "statusMessage": "hindsight: recalling memories"
          }
        ]
      },
      {
        "hooks": [
          {
            "type": "command",
            "command": "if [ -f '/o/claude-hook.sh' ]; then /bin/sh '/o/claude-hook.sh'; fi",
            "timeout": 10
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "python3 /r/hindsight/scripts/session_start.py",
            "timeout": 5
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "python3 /r/hindsight/scripts/retain.py",
            "timeout": 15,
            "async": true
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "python3 /r/hindsight/scripts/session_end.py",
            "timeout": 10
          }
        ]
      }
    ]
  },
  "statusLine": {
    "type": "command",
    "command": "sh hud.sh"
  },
  "enabledPlugins": {
    "ponytail@ponytail": true
  },
  "tui": "fullscreen"
}
"#;

fn fixture(settings_json: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join("data").join("memgarden")).unwrap();
    let config = tmp.path().join("memgarden.toml");
    std::fs::write(
        &config,
        format!(
            "[hooks]\nstate_dir = {:?}\n",
            tmp.path().join("state").to_string_lossy()
        ),
    )
    .unwrap();
    let settings = tmp.path().join("settings.json");
    std::fs::write(&settings, settings_json).unwrap();
    Fixture {
        _tmp: tmp,
        home,
        config,
        settings,
    }
}

impl Fixture {
    fn run(&self, args: &[&str]) -> Output {
        self.run_against(args, None)
    }

    fn run_against(&self, args: &[&str], daemon_url: Option<&str>) -> Output {
        let mut full: Vec<&str> = args.to_vec();
        let settings = self.settings.to_str().unwrap();
        full.extend_from_slice(&["--settings", settings]);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_memgarden"));
        cmd.args(&full)
            .env("MEMGARDEN_CONFIG", &self.config)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env_remove("MEMGARDEN_HOOKS_DISABLE")
            .env_remove("CLAUDE_CONFIG_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(url) = daemon_url {
            cmd.env("MEMGARDEN_DAEMON_URL", url);
        }
        cmd.spawn()
            .expect("spawn memgarden")
            .wait_with_output()
            .expect("wait")
    }

    /// The token `memgardend` would have written to `<data>/daemon.token`.
    fn plant_token(&self) {
        std::fs::write(self.home.join("data/memgarden/daemon.token"), TOKEN).unwrap();
    }

    fn plant_state(&self, session_id: &str, extra: serde_json::Value) {
        let dir = self.config.parent().unwrap().join("state");
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = serde_json::json!({
            "schema": 1,
            "session_id": session_id,
            "bank_id": "claude-code::demo",
            "offset": 0,
            "chunk": 0,
            "turns": 0,
            "turns_since_retain": 0,
            "compactions": 0,
            "transport_failures": 0,
            "reject_failures": 0,
            "breaker_open_until_ms": 0,
        });
        for (k, v) in extra.as_object().unwrap() {
            state[k] = v.clone();
        }
        std::fs::write(dir.join(format!("{session_id}.json")), state.to_string()).unwrap();
    }

    fn settings_bytes(&self) -> String {
        std::fs::read_to_string(&self.settings).unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_code(out: &Output, want: i32, what: &str) {
    assert_eq!(
        out.status.code(),
        Some(want),
        "{what}\nstdout: {}\nstderr: {}",
        stdout(out),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn install_then_uninstall_restores_the_file_byte_for_byte() {
    let f = fixture(WITH_LEGACY);
    let before = f.settings_bytes();

    let out = f.run(&["hooks", "install"]);
    assert_code(&out, 0, "install");
    let installed = f.settings_bytes();
    assert_ne!(installed, before);
    // Four entries, and the legacy ones are all still there.
    assert_eq!(installed.matches("memgarden: ").count(), 4);
    assert_eq!(
        before.matches("hindsight").count(),
        installed.matches("hindsight").count()
    );
    assert!(installed.contains("hud.sh"));

    let out = f.run(&["hooks", "uninstall"]);
    assert_code(&out, 0, "uninstall");
    assert_eq!(f.settings_bytes(), before, "uninstall did not restore");
}

/// The entries have to be the ones Claude Code will actually run: exec form
/// (so no `/bin/sh -c`), the plan's timeouts, and `async` on `Stop` only.
#[test]
fn the_installed_entries_are_the_exec_form_the_plan_specifies() {
    let f = fixture(WITH_LEGACY);
    assert_code(&f.run(&["hooks", "install"]), 0, "install");
    let doc: serde_json::Value = serde_json::from_str(&f.settings_bytes()).unwrap();

    let ours = |event: &str| -> serde_json::Value {
        doc["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .find(|h| {
                h["statusMessage"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("memgarden: "))
            })
            .cloned()
            .unwrap_or_else(|| panic!("no memgarden entry for {event}"))
    };

    for (event, sub, timeout) in [
        ("SessionStart", "session-start", 5),
        ("UserPromptSubmit", "recall", 10),
        ("Stop", "retain", 30),
        ("SessionEnd", "session-end", 5),
    ] {
        let h = ours(event);
        assert_eq!(h["type"], "command", "{event}");
        assert_eq!(h["args"], serde_json::json!(["hook", sub]), "{event}");
        assert_eq!(h["timeout"], timeout, "{event}");
        // Absent rather than `false` everywhere but `Stop`: the shortest
        // entry that says what it means, and Claude Code's own default.
        assert_eq!(
            h.get("async").and_then(|v| v.as_bool()).unwrap_or(false),
            event == "Stop",
            "{event}"
        );
        // The command is the binary itself and nothing else — no shell, no
        // env prefix, no quoting hazard.
        let command = h["command"].as_str().unwrap();
        assert!(
            Path::new(command).is_absolute() && command.ends_with("memgarden"),
            "{event}: {command}"
        );
    }
}

/// The refusal the whole coexistence design rests on. Legacy's strip list does
/// not know `<memgarden_memories>`, so full mode with legacy wired feeds our
/// injections into legacy's bank — and we cannot fix legacy.
#[test]
fn full_mode_refuses_while_legacy_is_wired_and_writes_nothing() {
    let f = fixture(WITH_LEGACY);
    let before = f.settings_bytes();

    let out = f.run(&["hooks", "install", "--mode", "full"]);
    assert_code(&out, 1, "install --mode full");
    assert_eq!(f.settings_bytes(), before, "a refusal wrote to the file");
    let printed = stdout(&out);
    assert!(printed.contains("refusing --mode full"), "{printed}");
    // It names them, per event, so the user can act on it.
    assert!(printed.contains("UserPromptSubmit"), "{printed}");
    assert!(printed.contains("retain.py"), "{printed}");

    // Explicitly overridden: proceeds.
    let out = f.run(&[
        "hooks",
        "install",
        "--mode",
        "full",
        "--allow-double-injection",
    ]);
    assert_code(&out, 0, "--allow-double-injection");
    assert_eq!(f.settings_bytes().matches("memgarden: ").count(), 4);

    // And it says, in the same breath, that the runtime mode did not move:
    // wiring and enabling are two layers and this command owns one of them.
    let printed = stdout(&out);
    assert!(printed.contains("mode = shadow"), "{printed}");
    assert!(printed.contains("does NOT change this"), "{printed}");
}

/// Shadow with legacy present is the *supported* configuration — the AC-1
/// instrument — so it must proceed without argument.
#[test]
fn shadow_mode_installs_alongside_legacy_without_complaint() {
    let f = fixture(WITH_LEGACY);
    let out = f.run(&["hooks", "install"]);
    assert_code(&out, 0, "install");
    assert!(!stdout(&out).contains("refusing"), "{}", stdout(&out));
    assert_eq!(f.settings_bytes().matches("memgarden: ").count(), 4);
}

#[test]
fn dry_run_prints_the_diff_and_writes_nothing() {
    let f = fixture(WITH_LEGACY);
    let before = f.settings_bytes();

    let out = f.run(&["hooks", "install", "--dry-run"]);
    assert_code(&out, 0, "install --dry-run");
    assert_eq!(f.settings_bytes(), before);
    let printed = stdout(&out);
    let added = printed.lines().filter(|l| l.starts_with('+')).count();
    assert_eq!(added, 4, "four + lines: {printed}");
    assert!(printed.contains("nothing written"), "{printed}");

    // Same for uninstall, on a file that does have entries.
    assert_code(&f.run(&["hooks", "install"]), 0, "install");
    let installed = f.settings_bytes();
    let out = f.run(&["hooks", "uninstall", "--dry-run"]);
    assert_code(&out, 0, "uninstall --dry-run");
    assert_eq!(f.settings_bytes(), installed);
    assert!(stdout(&out).contains("nothing written"));
}

/// Every write takes a timestamped copy first and prints where it went. That
/// is the recovery path for the one race the atomic write cannot close.
#[test]
fn a_backup_is_written_and_its_path_printed() {
    let f = fixture(WITH_LEGACY);
    let out = f.run(&["hooks", "install"]);
    assert_code(&out, 0, "install");

    let printed = stdout(&out);
    let path = printed
        .lines()
        .find_map(|l| l.strip_prefix("backup: "))
        .unwrap_or_else(|| panic!("no backup line in:\n{printed}"));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        WITH_LEGACY,
        "the backup is not the pre-install file"
    );
}

/// Installing twice is a no-op the second time. The file watcher makes every
/// write a live reconfiguration of every open Claude Code window, so a
/// needless write is not free.
#[test]
fn a_second_install_writes_nothing() {
    let f = fixture(WITH_LEGACY);
    assert_code(&f.run(&["hooks", "install"]), 0, "install");
    let installed = f.settings_bytes();

    let out = f.run(&["hooks", "install"]);
    assert_code(&out, 0, "second install");
    assert_eq!(f.settings_bytes(), installed);
    assert!(
        stdout(&out).contains("already installed"),
        "{}",
        stdout(&out)
    );
}

/// A settings.json that does not parse is refused, not repaired, and not
/// overwritten.
#[test]
fn a_broken_settings_file_is_refused_and_left_alone() {
    let broken = "{\n  \"hooks\": {\n";
    let f = fixture(broken);
    let out = f.run(&["hooks", "install"]);
    assert_code(&out, 1, "install on a broken file");
    assert_eq!(f.settings_bytes(), broken);
    assert!(stdout(&out).contains("does not parse"), "{}", stdout(&out));
}

/// A fresh Claude Code install has no settings.json. Creating one is better
/// than sending the user to hand-write JSON before they can use the switch.
#[test]
fn a_missing_settings_file_is_created() {
    let f = fixture("{}");
    std::fs::remove_file(&f.settings).unwrap();
    let out = f.run(&["hooks", "install"]);
    assert_code(&out, 0, "install with no settings.json");
    let doc: serde_json::Value = serde_json::from_str(&f.settings_bytes()).unwrap();
    assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
}

/// `status` is a diagnostic: it reports both systems, and it exits 0 whatever
/// it finds — including with no daemon running, which is the state on any
/// machine that has not started `memgardend` yet.
#[test]
fn status_reports_both_systems_and_always_exits_zero() {
    let f = fixture(WITH_LEGACY);
    let out = f.run(&["hooks", "status"]);
    assert_code(&out, 0, "status before install");
    let printed = stdout(&out);
    for event in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"] {
        assert!(printed.contains(event), "{event} missing:\n{printed}");
    }
    assert!(printed.matches("hindsight").count() >= 4, "{printed}");
    assert!(printed.contains("mode        shadow"), "{printed}");

    assert_code(&f.run(&["hooks", "install"]), 0, "install");
    let printed = stdout(&f.run(&["hooks", "status"]));
    assert_eq!(printed.matches("memgarden ").count(), 4, "{printed}");
    assert!(printed.contains("BOTH SYSTEMS ARE WIRED"), "{printed}");
    // The GPU warning is unconditional while both are wired; the
    // double-injection one is not, because shadow does not inject.
    assert!(printed.contains("GPU contention"), "{printed}");
    assert!(!printed.contains("DOUBLE INJECTION"), "{printed}");
}

/// `MEMGARDEN_HOOKS_DISABLE` silences the hooks. It must not silence the tool
/// that reports whether the hooks are wired — that is the state being asked
/// about.
#[test]
fn status_still_answers_when_the_hooks_are_disabled() {
    let f = fixture(WITH_LEGACY);
    let out = Command::new(env!("CARGO_BIN_EXE_memgarden"))
        .args([
            "hooks",
            "status",
            "--settings",
            f.settings.to_str().unwrap(),
        ])
        .env("MEMGARDEN_CONFIG", &f.config)
        .env("HOME", &f.home)
        .env("XDG_DATA_HOME", f.home.join("data"))
        .env("MEMGARDEN_HOOKS_DISABLE", "1")
        .output()
        .expect("run status");
    assert_code(&out, 0, "status with the hooks disabled");
    let printed = stdout(&out);
    assert!(printed.contains("MEMGARDEN_HOOKS_DISABLE"), "{printed}");
    assert!(printed.contains("SessionStart"), "{printed}");
}

/// The installer family may exit 1. It may never exit 2 — the guarantee is
/// crate-wide and a typo at a shell is exactly how a family that returns
/// codes starts returning the wrong one.
#[test]
fn a_bad_invocation_exits_one_and_never_two() {
    let f = fixture(WITH_LEGACY);
    // A real subcommand with a bad argument refuses visibly. `--settings`
    // with its value missing is in here because the silent fallback was to
    // the user's **real** `~/.claude/settings.json`.
    for args in [
        vec!["hooks", "install", "--dry-runn"],
        vec!["hooks", "install", "--mode", "loud"],
    ] {
        let out = f.run(&args);
        assert_code(&out, 1, &format!("{args:?}"));
        assert_eq!(
            f.settings_bytes(),
            WITH_LEGACY,
            "{args:?} wrote to the file"
        );
    }

    // Anything that is *not* one of the three subcommands falls through to the
    // crate's silent-zero arm instead. Only `install|uninstall|status` are
    // routed to this family — a typo in a hand-edited settings.json has to
    // stay a silent success on a hook event, which is what every unrecognised
    // argv did before this PR.
    for args in [vec!["hooks", "wat"], vec!["hooks"]] {
        let out = f.run(&args);
        assert_code(&out, 0, &format!("{args:?}"));
        assert!(out.stdout.is_empty(), "{args:?} printed {:?}", stdout(&out));
        assert_eq!(
            f.settings_bytes(),
            WITH_LEGACY,
            "{args:?} wrote to the file"
        );
    }
}

/// The number the shadow run's own re-entry criterion is measured in, and it
/// has to be visible **without** `[hooks] debug`. The open `chunks_failed`
/// cursor gap is otherwise reported only on a debug-gated stderr line, which
/// a default installation never prints — so a shadow run could not evaluate
/// the criterion it exists to produce.
#[test]
fn status_reports_unconfirmed_bytes_without_debug() {
    let f = fixture(WITH_LEGACY);
    f.plant_token();
    f.plant_state(
        "s-inflight",
        serde_json::json!({"offset": 5000, "chunk": 2}),
    );
    let url = stub(5000, 2000);

    let printed = stdout(&f.run_against(&["hooks", "status"], Some(&url)));
    assert!(printed.contains("inflight=3000 B"), "{printed}");
    assert!(printed.contains("unconfirmed 3000 B"), "{printed}");
    // And it says what the number is worth: the same defect that opens the gap
    // also shrinks this number, so a zero is not proof of convergence.
    assert!(printed.contains("LOWER BOUND"), "{printed}");
    // `debug` is false here — the default — which is the whole point.
    assert!(
        !std::fs::read_to_string(&f.config)
            .unwrap()
            .contains("debug"),
        "the fixture must not enable debug or this proves nothing"
    );
}

/// A locally-recorded `pending` is unconfirmed by definition and needs no
/// daemon to say so.
#[test]
fn status_reports_a_pending_job_with_no_daemon_at_all() {
    let f = fixture(WITH_LEGACY);
    f.plant_state(
        "s-pending",
        serde_json::json!({
            "offset": 9000,
            "pending": {"job_id": "job-7", "offset_from": 1000, "offset_to": 9000, "chunk_before": 1}
        }),
    );
    let printed = stdout(&f.run(&["hooks", "status"]));
    assert!(printed.contains("s-pending pending job=job-7"), "{printed}");
    assert!(printed.contains("bytes=8000"), "{printed}");
}

/// `--clear-poison` is the operator's way out of a session the daemon has
/// durably rejected, and it has to clear the counter as well as the stamp —
/// otherwise the next 4xx re-poisons immediately.
#[test]
fn clear_poison_clears_the_stamp_and_the_counter() {
    let f = fixture(WITH_LEGACY);
    let state_dir = f.config.parent().unwrap().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let state = serde_json::json!({
        "schema": 1,
        "session_id": "s-poisoned",
        "bank_id": "claude-code::demo",
        "offset": 10,
        "chunk": 1,
        "turns": 5,
        "turns_since_retain": 2,
        "compactions": 0,
        "transport_failures": 0,
        "reject_failures": 10,
        "breaker_open_until_ms": 0,
        "poisoned_at": 1_700_000_000_000i64,
    });
    let path = state_dir.join("s-poisoned.json");
    std::fs::write(&path, state.to_string()).unwrap();

    let printed = stdout(&f.run(&["hooks", "status"]));
    assert!(printed.contains("poisoned    1"), "{printed}");

    let out = f.run(&["hooks", "status", "--clear-poison", "s-poisoned"]);
    assert_code(&out, 0, "--clear-poison");
    assert!(
        stdout(&out).contains("s-poisoned cleared"),
        "{}",
        stdout(&out)
    );

    let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(after["poisoned_at"], serde_json::Value::Null);
    assert_eq!(after["reject_failures"], 0);
    // The cursor is untouched: clearing a poison is not a reason to re-ingest.
    assert_eq!(after["offset"], 10);
    assert_eq!(after["chunk"], 1);
}
