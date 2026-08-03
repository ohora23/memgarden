//! Bank id derivation (plan §Binding decisions #4).
//!
//! Ports `lib/bank.py:32-143` — `directoryBankMap` -> static -> dynamic
//! `agent::project` — with one deliberate change: **no `git` subprocess.**
//! Legacy shells out to
//! `git -C <cwd> rev-parse --path-format=absolute --git-common-dir` on every
//! hook invocation, measured at **0.435 ms p50** on this machine, which is
//! more than the entire rest of the hook. Walking up for `.git` is a handful
//! of `stat`s and gives the same answer for the two shapes that occur: a
//! normal repo and a linked worktree.
//!
//! The default is `claude-code::<project>` — byte-identical to the live legacy
//! bank ids (`claude-code::bank-b`,
//! `claude-code::bank e`), which is what lets AC-1 compare the
//! two systems on the same bank.

use std::path::{Path, PathBuf};

use memgarden_core::config::HooksConfig;

/// legacy: `bank.py:46` — an empty cwd yields this rather than an empty
/// segment, so a bank id is never `claude-code::`.
const UNKNOWN_PROJECT: &str = "unknown";

/// Derives the bank id for one hook invocation.
///
/// `dir` is the project directory: `CLAUDE_PROJECT_DIR` when set, else the
/// payload's `cwd` (see `HookInput::project_dir`).
pub fn derive(cfg: &HooksConfig, dir: &str) -> String {
    // 1. Explicit directory -> bank overrides win (`bank.py:87-101`).
    //
    // Skipped entirely when the map is empty, which is the default: each
    // comparison costs a `canonicalize()` syscall on both sides, and paying
    // that on the per-prompt path for a feature nobody configured would be
    // the same mistake as the `git` subprocess.
    if !dir.is_empty() && !cfg.directory_bank_map.is_empty() {
        let target = std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir));
        for (mapped_dir, bank_id) in &cfg.directory_bank_map {
            let mapped =
                std::fs::canonicalize(mapped_dir).unwrap_or_else(|_| PathBuf::from(mapped_dir));
            if mapped == target {
                return bank_id.clone();
            }
        }
    }

    // 2. Static mode: one bank for everything (`bank.py:103-106`).
    if !cfg.bank_id.is_empty() {
        return cfg.bank_id.clone();
    }

    // 3. Dynamic mode, granularity fixed at `agent::project`
    //    (`bank.py:109-111`, the legacy default). `session`, `channel` and
    //    `user` are not ported — a per-session bank defeats the point of a
    //    long-term memory, and the other two are Openclaw multi-tenant
    //    leftovers with no Claude Code caller (plan §parity-gaps).
    format!("{}::{}", cfg.agent_name, project_name(dir))
}

/// Basename of the repository root containing `dir`, or of `dir` itself when
/// it is not in a repo.
pub fn project_name(dir: &str) -> String {
    if dir.is_empty() {
        return UNKNOWN_PROJECT.to_string();
    }
    let path = Path::new(dir);
    let root = repo_root(path).unwrap_or_else(|| path.to_path_buf());
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| UNKNOWN_PROJECT.to_string())
}

/// Walks up from `start` looking for `.git`, and returns the **main**
/// repository's root — so every worktree of a repo shares one bank, which is
/// the behaviour `resolveWorktrees` gives legacy (`bank.py:48-68`).
fn repo_root(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        // `metadata`, which **follows symlinks**, not `symlink_metadata`.
        // A symlinked `.git` (`ln -s /repo/.git /proj/.git`) lstats as
        // not-a-dir, fell into the `gitdir:` branch, and `read_to_string` on a
        // directory returns EISDIR — which then abandoned the whole ancestor
        // walk and gave every subdirectory its own bank. Measured:
        // `derive(cfg, "/tmp/proj/sub")` was `claude-code::sub`, splitting one
        // project's memories across two banks depending on where the model
        // happened to be. `git rev-parse` handles it, so this had to too.
        let Ok(meta) = std::fs::metadata(&dot_git) else {
            continue;
        };

        if meta.is_dir() {
            return Some(dir.to_path_buf());
        }
        // `.git` as a *file* means a linked worktree or a submodule; it holds
        // `gitdir: <path>` pointing at the real git dir.
        //
        // An unreadable or shapeless `.git` file `continue`s rather than `?`s:
        // giving up on the entire walk because one ancestor was odd is how the
        // symlink bug above became a wrong bank instead of a slower lookup.
        let Some(gitdir) = std::fs::read_to_string(&dot_git)
            .ok()
            .and_then(|c| c.split_once("gitdir:").map(|(_, g)| g.trim().to_string()))
        else {
            continue;
        };
        let gitdir = gitdir.as_str();
        let gitdir = if Path::new(gitdir).is_absolute() {
            PathBuf::from(gitdir)
        } else {
            dir.join(gitdir)
        };
        // A worktree's git dir is `<common>/worktrees/<name>`; stripping those
        // two components is what `--git-common-dir` does, and the repo root is
        // the common dir's parent.
        if let Some(parent) = gitdir.parent()
            && parent.file_name().is_some_and(|n| n == "worktrees")
            && let Some(common) = parent.parent()
        {
            return common.parent().map(Path::to_path_buf);
        }
        // Anything else with a `gitdir:` file (a submodule, whose git dir is
        // `<super>/.git/modules/<name>`) resolves to the directory holding the
        // `.git` file. **Deliberately not legacy's answer** — legacy would
        // report `modules`, the basename of the parent of that git dir, which
        // is a bug we are not porting. Submodules are also not a shape this
        // repo or the live banks have.
        return Some(dir.to_path_buf());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg() -> HooksConfig {
        memgarden_core::config::Config::defaults().unwrap().hooks
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn a_plain_repo_yields_agent_and_repo_basename() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("myproject");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("crates/deep")).unwrap();

        assert_eq!(
            derive(&cfg(), repo.to_str().unwrap()),
            "claude-code::myproject"
        );
        // From a subdirectory too — the walk goes up.
        assert_eq!(
            derive(&cfg(), repo.join("crates/deep").to_str().unwrap()),
            "claude-code::myproject"
        );
    }

    /// The measured reason this module exists: a linked worktree must land in
    /// the *main* repo's bank, and it must do so without running `git`.
    #[test]
    fn a_linked_worktree_resolves_to_the_main_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("myproject");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt1")).unwrap();
        let wt = tmp.path().join("scratch-wt");
        write(
            &wt.join(".git"),
            &format!("gitdir: {}\n", repo.join(".git/worktrees/wt1").display()),
        );
        assert_eq!(
            derive(&cfg(), wt.to_str().unwrap()),
            "claude-code::myproject"
        );

        // The same, written relative — `git worktree add` inside the repo
        // tree produces this form.
        let wt2 = tmp.path().join("rel-wt");
        write(
            &wt2.join(".git"),
            "gitdir: ../myproject/.git/worktrees/wt1\n",
        );
        assert_eq!(
            derive(&cfg(), wt2.to_str().unwrap()),
            "claude-code::myproject"
        );
    }

    /// Review found this by measurement: with `symlink_metadata` a symlinked
    /// `.git` split one project across two banks depending on which
    /// subdirectory the model was in.
    #[test]
    fn a_symlinked_git_dir_still_resolves_to_the_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join(".git")).unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        std::os::unix::fs::symlink(real.join(".git"), proj.join(".git")).unwrap();

        assert_eq!(derive(&cfg(), proj.to_str().unwrap()), "claude-code::proj");
        // The subdirectory is the case that regressed: it must not become
        // `claude-code::sub`.
        assert_eq!(
            derive(&cfg(), proj.join("sub").to_str().unwrap()),
            "claude-code::proj"
        );
    }

    /// One odd ancestor must not abandon the walk — the mechanism behind the
    /// symlink bug, pinned separately from the symlink itself.
    #[test]
    fn an_unreadable_git_entry_does_not_abandon_the_ancestor_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("myproject");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let inner = repo.join("vendor");
        // A `.git` file with no `gitdir:` line at all.
        write(&inner.join(".git"), "this is not a git link\n");
        assert_eq!(
            derive(&cfg(), inner.to_str().unwrap()),
            "claude-code::myproject"
        );
    }

    #[test]
    fn a_gitdir_file_that_is_not_a_worktree_uses_its_own_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("super/vendor/libfoo");
        write(&sub.join(".git"), "gitdir: ../../.git/modules/libfoo\n");
        assert_eq!(derive(&cfg(), sub.to_str().unwrap()), "claude-code::libfoo");
    }

    #[test]
    fn outside_a_repo_it_falls_back_to_the_directory_basename() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("just-a-dir");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            derive(&cfg(), plain.to_str().unwrap()),
            "claude-code::just-a-dir"
        );
        // legacy bank.py:46 — an empty cwd is "unknown", never an empty segment.
        assert_eq!(derive(&cfg(), ""), "claude-code::unknown");
        assert_eq!(derive(&cfg(), "/"), "claude-code::unknown");
    }

    #[test]
    fn directory_bank_map_beats_everything_and_is_matched_after_canonicalization() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("myproject");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let mut c = cfg();
        // Even with a static bank_id set, the map wins (bank.py:87 runs first).
        c.bank_id = "pinned".to_string();
        c.directory_bank_map = HashMap::from([(
            // Deliberately non-canonical: an extra `.` component, which is
            // exactly what `realpath` normalization is for in legacy.
            repo.join(".").to_string_lossy().into_owned(),
            "mapped-bank".to_string(),
        )]);
        assert_eq!(derive(&c, repo.to_str().unwrap()), "mapped-bank");

        // A directory that is not in the map falls through to static mode.
        let other = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(derive(&c, other.to_str().unwrap()), "pinned");
    }

    #[test]
    fn a_static_bank_id_pins_every_directory() {
        let mut c = cfg();
        c.bank_id = "one-bank".to_string();
        assert_eq!(derive(&c, "/anywhere"), "one-bank");
        assert_eq!(derive(&c, ""), "one-bank");
    }

    #[test]
    fn the_agent_segment_is_configurable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = cfg();
        c.agent_name = "codex".to_string();
        assert_eq!(derive(&c, dir.to_str().unwrap()), "codex::proj");
    }

    /// The bank id goes into a URL path, and both live shapes need escaping.
    #[test]
    fn derived_ids_survive_the_url_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bank e");
        std::fs::create_dir_all(&dir).unwrap();
        let id = derive(&cfg(), dir.to_str().unwrap());
        assert_eq!(id, "claude-code::bank e");
        assert_eq!(
            crate::http::encode_path_segment(&id),
            "claude-code%3A%3Abank%20e"
        );
    }
}
