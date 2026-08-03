//! The `memgarden` binary. See the crate docs in `lib.rs` for why every path
//! here ends in exit 0.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Installed before anything else runs, so there is no window in which a
    // panic takes the default hook's exit code (101).
    //
    // `process::exit(0)` rather than letting the unwind finish: it is the
    // shortest path to the guarantee, and it deliberately skips Rust's
    // end-of-main stdout flush. A subcommand that panicked half way through
    // writing `additionalContext` therefore emits **nothing** instead of a
    // truncated JSON line that Claude Code would hand to the model. Empty is
    // the correct partial result for every one of our events.
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(std::io::stderr(), "memgarden: {info}");
        std::process::exit(0);
    }));

    let args: Vec<String> = std::env::args().skip(1).collect();
    memgarden_cli::dispatch(&args);

    // The only `return` in the program. No `?`, no `std::process::exit` with
    // anything but 0, no `unwrap` outside the panic hook's reach.
    ExitCode::SUCCESS
}
