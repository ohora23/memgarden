//! Where the listening socket comes from (DP-1 D1).
//!
//! Under systemd socket activation the unit owns `127.0.0.1:9100` and hands
//! it to the daemon as file descriptor 3; the socket stays open across a
//! restart, so a hook that arrives while the process is being replaced waits
//! in the kernel backlog instead of seeing `ECONNREFUSED`. Absent systemd —
//! `cargo run`, the tests, a terminal tab — the daemon binds `cfg.bind` as it
//! always has.
//!
//! The protocol is two environment variables: `LISTEN_FDS` (how many fds,
//! starting at 3) and `LISTEN_PID` (which process they were meant for). The
//! PID check is the part that matters: a child of the daemon inherits the
//! environment and must not steal the socket.

use std::os::fd::FromRawFd;

/// The first activation fd, exactly when systemd handed this process one.
///
/// Pure so it can be tested without touching the process environment:
/// `LISTEN_PID` must equal `my_pid` and `LISTEN_FDS` must be at least 1.
pub fn activated_fd(
    listen_pid: Option<&str>,
    listen_fds: Option<&str>,
    my_pid: u32,
) -> Option<i32> {
    let pid: u32 = listen_pid?.trim().parse().ok()?;
    let fds: u32 = listen_fds?.trim().parse().ok()?;
    (pid == my_pid && fds >= 1).then_some(3)
}

/// The listener: inherited from systemd when activated, bound otherwise.
/// Returns the listener and where it came from, for the startup log line.
pub async fn listener(bind: &str) -> std::io::Result<(tokio::net::TcpListener, &'static str)> {
    let fd = activated_fd(
        std::env::var("LISTEN_PID").ok().as_deref(),
        std::env::var("LISTEN_FDS").ok().as_deref(),
        std::process::id(),
    );
    match fd {
        Some(fd) => {
            // SAFETY: systemd created this fd as a listening socket for this
            // exact process (`LISTEN_PID` matched above); nothing else in the
            // daemon owns or closes fd 3.
            let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
            std_listener.set_nonblocking(true)?;
            Ok((
                tokio::net::TcpListener::from_std(std_listener)?,
                "systemd socket",
            ))
        }
        None => Ok((tokio::net::TcpListener::bind(bind).await?, "bound")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_matching_pid_with_at_least_one_fd_activates() {
        assert_eq!(activated_fd(Some("42"), Some("1"), 42), Some(3));
        assert_eq!(activated_fd(Some("42"), Some("2"), 42), Some(3));
        // A child inherits LISTEN_PID from its parent and must not take fd 3.
        assert_eq!(activated_fd(Some("42"), Some("1"), 43), None);
        assert_eq!(activated_fd(Some("42"), Some("0"), 42), None);
        assert_eq!(activated_fd(None, Some("1"), 42), None);
        assert_eq!(activated_fd(Some("42"), None, 42), None);
        assert_eq!(activated_fd(Some("x"), Some("1"), 42), None);
    }

    #[tokio::test]
    async fn without_systemd_it_binds_the_configured_address() {
        // The test process has no LISTEN_* variables; a fresh port proves the
        // fallback is a real bind and not fd 3.
        let (l, source) = listener("127.0.0.1:0").await.unwrap();
        assert_eq!(source, "bound");
        assert_ne!(l.local_addr().unwrap().port(), 0);
    }
}
