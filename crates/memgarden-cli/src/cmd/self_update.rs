//! `memgarden self-update [--version <tag>] [--dry-run]` — the adopter's
//! deploy (DP-1 §8). Download the release asset for this target, verify its
//! sha256, refuse a build older than the database, back up if the schema
//! moves, install both binaries by rename, restart, verify by build.
//!
//! No new crate: the hook binary's dependency closure is CI-enforced, and
//! this path is run by a person, not by a hook. `curl`, `tar` and
//! `sha256sum` do the network and the archive; the approval is the Bash
//! permission prompt the skill leaves in place.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const REPO: &str = "ohora23/memgarden";
const SERVICE: &str = "memgardend.service";
const BINARIES: [&str; 2] = ["memgardend", "memgarden"];

/// The two asset URLs a release must carry for this target.
#[derive(Debug, PartialEq)]
pub struct Release {
    pub tag: String,
    pub tarball: String,
    pub sha256: String,
}

pub fn asset_name(tag: &str, target: &str) -> String {
    format!("memgarden-{tag}-{target}.tar.gz")
}

/// Picks this target's tarball and checksum out of a GitHub release object.
pub fn pick(release: &serde_json::Value, target: &str) -> Result<Release, String> {
    let tag = release["tag_name"]
        .as_str()
        .ok_or("release has no tag_name")?
        .to_string();
    let want = asset_name(&tag, target);
    let want_sha = format!("{want}.sha256");
    let (mut tarball, mut sha256) = (None, None);
    for asset in release["assets"].as_array().into_iter().flatten() {
        let name = asset["name"].as_str().unwrap_or("");
        let url = asset["browser_download_url"].as_str().unwrap_or("");
        if name == want {
            tarball = Some(url.to_string());
        } else if name == want_sha {
            sha256 = Some(url.to_string());
        }
    }
    Ok(Release {
        tarball: tarball.ok_or(format!("release {tag} has no asset {want}"))?,
        sha256: sha256.ok_or(format!("release {tag} has no asset {want_sha}"))?,
        tag,
    })
}

/// `schema vN` out of `memgardend --version`.
pub fn schema_of(version_line: &str) -> Option<i64> {
    version_line
        .split("schema v")
        .nth(1)?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
}

fn sh(dir: Option<&Path>, program: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn healthz() -> Option<serde_json::Value> {
    let cfg = memgarden_core::config::Config::load().ok()?;
    let target = crate::cmd::target(&cfg.hooks).ok()?;
    let timeouts = crate::cmd::interactive_timeouts(&cfg.hooks);
    let r = crate::http::get(&target, "/healthz", &timeouts).ok()?;
    serde_json::from_slice(&r.body).ok()
}

fn update(argv: &[String]) -> Result<(), String> {
    let mut version: Option<String> = None;
    let mut dry_run = false;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--version" => version = Some(it.next().ok_or("--version needs a tag")?.clone()),
            "--dry-run" => dry_run = true,
            "--snooze" => {
                let days: i64 = it
                    .next()
                    .and_then(|d| d.parse().ok())
                    .ok_or("--snooze needs a number of days")?;
                let cfg = memgarden_core::config::Config::load().map_err(|e| e.to_string())?;
                super::update_check::snooze(&cfg.hooks.state_dir, days, memgarden_core::now_ms());
                println!("update notices snoozed for {days} day(s)");
                return Ok(());
            }
            other => {
                return Err(format!(
                    "unknown argument {other}; usage: memgarden self-update [--version <tag>] [--dry-run] [--snooze <days>]"
                ));
            }
        }
    }

    // 1. What is out there, what is here.
    let api = match &version {
        Some(tag) => format!("https://api.github.com/repos/{REPO}/releases/tags/{tag}"),
        None => format!("https://api.github.com/repos/{REPO}/releases/latest"),
    };
    let body = sh(
        None,
        "curl",
        &[
            "-fsSL",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: memgarden-self-update",
            &api,
        ],
    )?;
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("release JSON: {e}"))?;
    let rel = pick(&json, memgarden_core::TARGET)?;
    println!(
        "release {}; this binary is {}",
        rel.tag,
        memgarden_core::BUILD
    );
    if rel.tag == memgarden_core::BUILD {
        println!("already up to date");
        return Ok(());
    }
    if dry_run {
        println!("--dry-run: would download {}", rel.tarball);
        return Ok(());
    }

    // 2. Download and verify.
    let tmp = std::env::temp_dir().join(format!("memgarden-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let name = asset_name(&rel.tag, memgarden_core::TARGET);
    sh(Some(&tmp), "curl", &["-fsSL", "-o", &name, &rel.tarball])?;
    sh(
        Some(&tmp),
        "curl",
        &["-fsSL", "-o", &format!("{name}.sha256"), &rel.sha256],
    )?;
    sh(Some(&tmp), "sha256sum", &["-c", &format!("{name}.sha256")])?;
    println!("sha256 verified: {name}");
    sh(Some(&tmp), "tar", &["-xzf", &name])?;
    for b in BINARIES {
        if !tmp.join(b).is_file() {
            return Err(format!("archive has no {b}"));
        }
    }

    // 3. Schema, before anything is installed: refuse an older build, back
    //    up ahead of a newer one.
    let new_daemon = tmp.join("memgardend");
    let version_line = sh(None, &new_daemon.to_string_lossy(), &["--version"])?;
    print!("{version_line}");
    let want =
        schema_of(&version_line).ok_or("cannot read the schema version from the new binary")?;
    let live = healthz();
    if let Some(live) = &live {
        let have = live["schema_version"].as_i64().unwrap_or(want);
        let db = live["db_path"].as_str().unwrap_or("");
        if want < have {
            return Err(format!(
                "release {} wants schema v{want} but the database is at v{have}: an older build would refuse to start. Restore the pre-v{have} backup first.",
                rel.tag
            ));
        }
        if want > have && !db.is_empty() {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let backup = Path::new(db).with_file_name(format!("backup-pre-v{want}-{secs}.db"));
            let out = sh(
                None,
                &new_daemon.to_string_lossy(),
                &["--backup-to", &backup.to_string_lossy()],
            )?;
            print!("{out}");
        }
    } else {
        println!("daemon not answering; no schema check possible, installing anyway");
    }

    // 4. Install beside the running binary, by rename; previous kept as .prev.
    let bin_dir: PathBuf = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("cannot resolve the install directory")?
        .to_path_buf();
    for b in BINARIES {
        let dst = bin_dir.join(b);
        let staged = bin_dir.join(format!("{b}.new"));
        std::fs::copy(tmp.join(b), &staged).map_err(|e| format!("{}: {e}", staged.display()))?;
        if dst.exists() {
            std::fs::rename(&dst, bin_dir.join(format!("{b}.prev"))).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&staged, &dst).map_err(|e| e.to_string())?;
        println!("installed {}", dst.display());
    }
    let _ = std::fs::remove_dir_all(&tmp);

    // 5. Restart, 6. verify by build.
    let under_systemd = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", SERVICE])
        .status()
        .is_ok_and(|s| s.success());
    if under_systemd {
        sh(None, "systemctl", &["--user", "restart", SERVICE])?;
        for _ in 0..60 {
            if let Some(h) = healthz()
                && h["build"].as_str() == Some(rel.tag.as_str())
            {
                println!(
                    "/healthz status={} build={}",
                    h["status"].as_str().unwrap_or("?"),
                    rel.tag
                );
                println!("updated to {}", rel.tag);
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        Err(format!(
            "restarted, but /healthz does not report build {} — check `journalctl --user -u {SERVICE}`",
            rel.tag
        ))
    } else {
        println!(
            "{SERVICE} is not running under systemd --user; restart the daemon yourself: {}",
            bin_dir.join("memgardend").display()
        );
        Ok(())
    }
}

pub fn run(argv: &[String]) -> ExitCode {
    match update(argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            println!("self-update: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_wants_exactly_this_targets_tarball_and_checksum() {
        let rel = serde_json::json!({
            "tag_name": "v0.1.0",
            "assets": [
                {"name": "memgarden-v0.1.0-x86_64-unknown-linux-gnu.tar.gz", "browser_download_url": "https://x/t"},
                {"name": "memgarden-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256", "browser_download_url": "https://x/s"},
                {"name": "memgarden-v0.1.0-aarch64-apple-darwin.tar.gz", "browser_download_url": "https://x/other"}
            ]
        });
        assert_eq!(
            pick(&rel, "x86_64-unknown-linux-gnu").unwrap(),
            Release {
                tag: "v0.1.0".into(),
                tarball: "https://x/t".into(),
                sha256: "https://x/s".into()
            }
        );
        let err = pick(&rel, "aarch64-apple-darwin").unwrap_err();
        assert!(err.contains("sha256"), "{err}");
        assert!(
            pick(&serde_json::json!({}), "x")
                .unwrap_err()
                .contains("tag_name")
        );
    }

    #[test]
    fn schema_is_read_from_the_version_line() {
        assert_eq!(
            schema_of("memgardend 0.1.0 (build v0.1.0, schema v13)\n"),
            Some(13)
        );
        assert_eq!(schema_of("memgardend 0.1.0"), None);
    }
}
