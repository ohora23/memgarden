// The build identity (DP-1 D3): the short git SHA, `-dirty` when the tree
// has uncommitted changes, `unknown` without a repository. `MEMGARDEN_BUILD`
// in the environment wins, so a release workflow can stamp a tag instead.
//
// Both binaries get it through `memgarden_core::BUILD`, which is what lets
// `/healthz` and the hook's request header be compared by `hooks status`.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=MEMGARDEN_BUILD");
    // A new commit or a staged change moves one of these.
    for p in ["../../.git/HEAD", "../../.git/index"] {
        println!("cargo:rerun-if-changed={p}");
    }
    let build = std::env::var("MEMGARDEN_BUILD").ok().unwrap_or_else(|| {
        match git(&["rev-parse", "--short", "HEAD"]) {
            Some(sha) => {
                let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
                if dirty { format!("{sha}-dirty") } else { sha }
            }
            None => "unknown".to_string(),
        }
    });
    println!("cargo:rustc-env=MEMGARDEN_BUILD={build}");
    // The triple this binary is for, so `self-update` can name its release asset.
    println!(
        "cargo:rustc-env=MEMGARDEN_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string())
    );
}
