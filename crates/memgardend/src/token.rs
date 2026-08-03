//! The daemon's identity token: `<data>/daemon.token`.
//!
//! # Which direction this authenticates, and why it is that one
//!
//! The token is stamped on every **response** and checked by the hook. It is
//! **not** sent on requests, and that is a correctness requirement rather than
//! a simplification.
//!
//! The threat this closes was demonstrated against C3: an impostor listener on
//! 127.0.0.1:9100 returned an `injected_text` containing a forged
//! `</memgarden_memories>` and a fake system-reminder, and it reached the
//! model's `additionalContext` verbatim. The daemon's own `defang` never runs,
//! because the impostor is not the daemon. `Target::parse` and `check_host`
//! both answer *"am I talking to loopback"*; neither answers *"am I talking to
//! memgardend"*. 9100 is unprivileged, nothing sets `SO_REUSEPORT`, so
//! `while ! bind; do sleep 1; done` from any local uid captures it the next
//! time this daemon restarts.
//!
//! A request header would authenticate the **client to the server** — the
//! opposite direction — and would not close that at all: the impostor simply
//! ignores it and answers 200. Worse, the two directions are **mutually
//! exclusive on one shared secret**: a client that sends the token hands it to
//! the impostor, which can then echo it back and pass the response check. So
//! the secret travels one way only, and it is the way that protects the
//! model's context.
//!
//! What this therefore does **not** do: stop another local process reading or
//! writing the bank. That is a pre-existing property of C2a/C2b, unchanged
//! here, and it needs a second secret rather than a second use of this one.
//!
//! The file is 0600 next to the database the daemon already chmods, so a uid
//! that cannot read our files cannot forge the header. A uid that *can* read
//! them can also write the state files and the config, so no token defends
//! against it.

use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

/// The response header. Named for the daemon, not for the transport, because
/// what it asserts is "this response came from memgardend".
pub const TOKEN_HEADER: &str = "x-memgarden-token";

/// 32 bytes, hex-encoded. Long enough that guessing is not a strategy and
/// short enough to sit in a header without wrapping.
const TOKEN_BYTES: usize = 32;

/// Set once at startup. A `OnceLock` rather than an `AppState` field on
/// purpose: the thirteen test call sites that build an `AppState` by hand get
/// `None` here and therefore an unstamped response, which is exactly what an
/// in-process router test wants — and adding a field would have made this
/// change touch every one of them for no behaviour.
static TOKEN: OnceLock<String> = OnceLock::new();

/// Reads `<data>/daemon.token`, creating it if absent, and pins it for the
/// process.
///
/// **Read-or-create, never regenerate.** A daemon restart must not invalidate
/// the token, because hooks are launched by Claude Code and never reloaded:
/// rotating it would silently blind every live session until the next hook
/// process happened to read the file again.
pub fn init(path: &Path) -> std::io::Result<()> {
    let token = match std::fs::read_to_string(path) {
        Ok(existing) if !existing.trim().is_empty() => existing.trim().to_string(),
        _ => {
            let token = generate()?;
            write_private(path, &token)?;
            token
        }
    };
    let _ = TOKEN.set(token);
    Ok(())
}

/// The token for this process, or `None` when `init` was never called — which
/// is every in-process test.
pub fn current() -> Option<&'static str> {
    TOKEN.get().map(String::as_str)
}

/// 32 bytes from `/dev/urandom`, hex-encoded.
///
/// The stdlib has no CSPRNG and `getrandom` is only a transitive dependency
/// here; `/dev/urandom` is the platform feature that already answers this and
/// costs one `open` at startup. `// ponytail:` swap for `getrandom` if this
/// ever needs to build for a target without it.
fn generate() -> std::io::Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `create_new` at 0600, so the open fails rather than following a symlink
/// someone planted at the path — the same rule the hook's state writer
/// follows, and for the same reason: this file is a secret and `File::create`
/// would write it through a link.
fn write_private(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        memgarden_core::paths::ensure_data_dir(parent).map_err(std::io::Error::other)?;
    }
    let mut options = std::fs::File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(token.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_thirty_two_hex_encoded_bytes_and_never_repeats() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// Read-or-create, and the "read" half is the one that matters: a restart
    /// that regenerated the token would blind every hook already launched.
    #[test]
    fn an_existing_token_is_reused_rather_than_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.token");
        write_private(&path, "deadbeef").unwrap();
        init(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "deadbeef");
    }

    #[cfg(unix)]
    #[test]
    fn a_new_token_file_is_0600_and_does_not_follow_a_planted_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.token");
        write_private(&path, "abc123").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // A secret must never be written through a link into a file someone
        // else can read.
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let planted = dir.path().join("planted.token");
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        assert!(write_private(&planted, "secret").is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
    }
}
