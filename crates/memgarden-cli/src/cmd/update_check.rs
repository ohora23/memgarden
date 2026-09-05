//! The "a newer release exists" signal (DP-1 §8).
//!
//! Two halves, deliberately apart. `hook update-check` is the **detached
//! child** `session-start` spawns beside `catchup`: at most once a day it
//! asks GitHub for the latest release and writes what it learned to
//! `<state_dir>/update-check.json`. Nothing on a hook's own clock ever
//! touches the network. `notice` is the **read side** the recall hook calls:
//! one file read, and a `systemMessage` when the cached release is newer
//! than this binary, not snoozed, and not already said today.
//!
//! "Newer than this binary" is the release's `published_at` against the
//! binary's own mtime — which works for a source build (`0b1f436`) as well
//! as a release build (`v0.1.0`), where a tag comparison would not.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const REPO: &str = "ohora23/memgarden";
const CACHE_FILE: &str = "update-check.json";
const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cache {
    #[serde(default)]
    pub checked_at_ms: i64,
    #[serde(default)]
    pub latest_tag: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub published_at_ms: i64,
    #[serde(default)]
    pub notified_at_ms: i64,
    #[serde(default)]
    pub snooze_until_ms: i64,
}

pub fn cache_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CACHE_FILE)
}

pub fn read(state_dir: &Path) -> Cache {
    std::fs::read(cache_path(state_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write(state_dir: &Path, cache: &Cache) {
    let path = cache_path(state_dir);
    let tmp = path.with_extension("json.tmp");
    if let Ok(body) = serde_json::to_vec(cache)
        && std::fs::write(&tmp, body).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// `2026-09-05T16:35:12Z` -> epoch milliseconds. GitHub's timestamps are
/// always UTC with a `Z`; anything else is `None`, which reads as "unknown"
/// and never triggers a notice.
pub fn parse_iso8601_utc(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':').map(|p| p.parse::<i64>());
    let (hh, mm, ss) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400) + hh * 3600 + mm * 60 + ss) * 1000)
}

/// The decision, pure: is there something to say, given what the cache
/// holds, what this binary is, and when it was installed?
pub fn should_notify(cache: &Cache, now_ms: i64, my_build: &str, my_mtime_ms: i64) -> bool {
    !cache.latest_tag.is_empty()
        && cache.latest_tag != my_build
        && cache.published_at_ms > my_mtime_ms
        && now_ms >= cache.snooze_until_ms
        && now_ms - cache.notified_at_ms >= DAY_MS
}

/// The read side. Returns the `systemMessage` text and records that it was
/// said, so the next one is a day away.
pub fn notice(state_dir: &Path, now_ms: i64) -> Option<String> {
    let mut cache = read(state_dir);
    let my_mtime_ms = std::env::current_exe()
        .and_then(|p| p.metadata())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(i64::MAX);
    if !should_notify(&cache, now_ms, memgarden_core::BUILD, my_mtime_ms) {
        return None;
    }
    cache.notified_at_ms = now_ms;
    write(state_dir, &cache);
    Some(format!(
        "MemGarden {} is available (this install is {}). `memgarden self-update` installs it — \
         it backs up before any schema change and restarts the daemon. \
         `memgarden self-update --snooze 7` to defer. {}",
        cache.latest_tag,
        memgarden_core::BUILD,
        cache.html_url
    ))
}

/// `self-update --snooze <days>`.
pub fn snooze(state_dir: &Path, days: i64, now_ms: i64) {
    let mut cache = read(state_dir);
    cache.snooze_until_ms = now_ms + days * DAY_MS;
    write(state_dir, &cache);
}

/// The detached child. Once a day, one request, one file.
pub fn run() {
    let Ok(cfg) = memgarden_core::config::Config::load() else {
        return;
    };
    let dir = cfg.hooks.state_dir.as_path();
    let now_ms = memgarden_core::now_ms();
    let mut cache = read(dir);
    if now_ms - cache.checked_at_ms < DAY_MS {
        return;
    }
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let Ok(out) = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "-m",
            "5",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: memgarden-update-check",
            &url,
        ])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return;
    };
    let Some(tag) = json["tag_name"].as_str() else {
        return;
    };
    cache.checked_at_ms = now_ms;
    cache.latest_tag = tag.to_string();
    cache.html_url = json["html_url"].as_str().unwrap_or("").to_string();
    cache.published_at_ms = json["published_at"]
        .as_str()
        .and_then(parse_iso8601_utc)
        .unwrap_or(0);
    write(dir, &cache);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_utc_parses_github_timestamps() {
        assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601_utc("2000-03-01T00:00:00Z"),
            Some(951_868_800_000)
        );
        assert_eq!(
            parse_iso8601_utc("2026-09-05T16:35:12Z"),
            Some(1_788_626_112_000)
        );
        assert_eq!(parse_iso8601_utc("2026-09-05T16:35:12+09:00"), None);
        assert_eq!(parse_iso8601_utc("garbage"), None);
    }

    #[test]
    fn notify_only_for_a_newer_release_not_snoozed_not_said_today() {
        let base = Cache {
            checked_at_ms: 1_000 * DAY_MS,
            latest_tag: "v0.2.0".into(),
            html_url: "https://x".into(),
            published_at_ms: 999 * DAY_MS,
            notified_at_ms: 0,
            snooze_until_ms: 0,
        };
        let now = 1_000 * DAY_MS;
        let installed = 998 * DAY_MS;
        assert!(should_notify(&base, now, "v0.1.0", installed));
        assert!(
            should_notify(&base, now, "0b1f436", installed),
            "source builds compare by time"
        );
        assert!(!should_notify(&base, now, "v0.2.0", installed), "same tag");
        assert!(
            !should_notify(&base, now, "v0.1.0", 999 * DAY_MS + 1),
            "binary newer than the release"
        );
        assert!(
            !should_notify(&Cache::default(), now, "v0.1.0", installed),
            "empty cache"
        );
        let snoozed = Cache {
            snooze_until_ms: now + 1,
            ..base.clone()
        };
        assert!(!should_notify(&snoozed, now, "v0.1.0", installed));
        let said = Cache {
            notified_at_ms: now - DAY_MS + 1,
            ..base.clone()
        };
        assert!(!should_notify(&said, now, "v0.1.0", installed));
        let said_yesterday = Cache {
            notified_at_ms: now - DAY_MS,
            ..base
        };
        assert!(should_notify(&said_yesterday, now, "v0.1.0", installed));
    }

    #[test]
    fn cache_round_trips_and_snooze_writes() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), Cache::default());
        snooze(dir.path(), 7, 10 * DAY_MS);
        assert_eq!(read(dir.path()).snooze_until_ms, 17 * DAY_MS);
    }
}
