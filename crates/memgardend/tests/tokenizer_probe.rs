//! A reproducer aimed at `tiktoken_rs::_byte_pair_merge_large`, which the
//! 2026-08-10 heap hunt caught corrupting a `BinaryHeap`.
//!
//! What was caught, once in 960 concurrent `retain_api` runs
//! (`RUST_BACKTRACE=full`, trimmed to the frames that matter):
//!
//! ```text
//! thread 'tokio-rt-worker' panicked at library/alloc/src/collections/binary_heap/mod.rs:1509:
//!   unsafe precondition(s) violated: slice::get_unchecked requires that the
//!   index is within the slice
//!   ...
//!   alloc::collections::binary_heap::Hole<T>::new
//!   alloc::collections::binary_heap::BinaryHeap<T,A>::sift_up
//!   alloc::collections::binary_heap::BinaryHeap<T,A>::push
//!   tiktoken_rs::vendor_tiktoken::_byte_pair_merge_large
//!   tiktoken_rs::vendor_tiktoken::CoreBPE::encode_with_special_tokens
//!   memgardend::retain::token_count
//!   memgardend::retain::plan_ingest
//!   memgardend::routes::retain::prepare
//! ```
//!
//! `BinaryHeap::push` passes `sift_up` the length *before* the push, so
//! `Hole::new` cannot be out of bounds on a heap whose memory is intact. The
//! heap is a local in `_byte_pair_merge_large`. So this frame is where the
//! damage is *observed*, not necessarily where it is done — and the second UB
//! signature on record, a `regex-automata` lazy-DFA `get_unchecked`, is
//! reachable from the same `encode` call (the regex split that precedes the
//! merge). Two independent out-of-bounds sites under one function is what
//! makes this path worth a probe of its own.
//!
//! # Why the first version of this probe found nothing
//!
//! `byte_pair_encode` only reaches `_byte_pair_merge_large` when a **single
//! regex-split piece is ≥ 100 bytes** (`vendor_tiktoken.rs:210`). Ordinary
//! prose splits into words, so the first probe — 16 threads hammering
//! `token_count` on synthetic code and prose, 40 processes — never entered
//! that path at all and reported 0 deaths. That was a measurement of the
//! wrong function.
//!
//! The text below is built to enter it: unbroken runs the cl100k pattern has
//! no boundary for. Korean is the realistic case rather than a synthetic one
//! — CJK has no word boundaries, and this project's transcripts are largely
//! Korean, which is how production reaches this path at all.
//!
//! # Harness
//!
//! ```text
//! cargo test -p memgardend --test tokenizer_probe --no-run
//! BIN=$(ls -t target/debug/deps/tokenizer_probe-* | grep -v '\.d$' | head -1)
//! for r in $(seq 20); do
//!   for i in $(seq 8); do $BIN --ignored & done; wait
//! done
//! ```
//!
//! A death is a signal, not a failed assertion: 134 if the std precondition
//! check fires, 139 if it does not and the read lands somewhere real.
//!
//! **A zero here is weaker than it looks.** This probe returned 0 in 800 runs
//! while the full `retain_api` suite was dying at roughly 0.4%, which is what
//! took `tiktoken` out of the picture. But the same day the suite's own rate
//! was measured swinging between 0% and 30% for an unchanged binary, so a zero
//! taken at one hour does not exonerate anything measured at another. See
//! `roadmap.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A piece the cl100k pattern will not split, comfortably over the 100-byte
/// threshold that selects `_byte_pair_merge_large`. `seed` varies the content so
/// no two calls walk the same merge sequence.
fn long_pieces(seed: u64) -> String {
    let mut s = String::with_capacity(8192);
    let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().collect();
    let hangul: Vec<char> = "가나다라마바사아자차카타파하거너더러머버서어저처커터퍼허"
        .chars()
        .collect();
    for i in 0..24 {
        let n = seed.wrapping_mul(2_654_435_761).wrapping_add(i);
        // A 300-char run of letters: one piece, no boundary inside it.
        let a: String = (0..300)
            .map(|k| alphabet[((n as usize).wrapping_add(k * 7)) % alphabet.len()])
            .collect();
        // A 400-char Korean run: the realistic shape, and CJK has no word
        // boundaries for the pattern to split on.
        let h: String = (0..400)
            .map(|k| hangul[((n as usize).wrapping_add(k * 11)) % hangul.len()])
            .collect();
        s.push_str(&a);
        s.push(' ');
        s.push_str(&h);
        s.push('\n');
    }
    s
}

fn hammer(threads: usize, iters: u64) {
    let total = Arc::new(AtomicU64::new(0));
    std::thread::scope(|scope| {
        for t in 0..threads {
            let total = Arc::clone(&total);
            scope.spawn(move || {
                for i in 0..iters {
                    let text = long_pieces((t as u64) << 32 | i);
                    total.fetch_add(memgardend::retain::token_count(&text), Ordering::Relaxed);
                }
            });
        }
    });
    assert!(
        total.load(Ordering::Relaxed) > 0,
        "the tokenizer counted nothing"
    );
}

/// Single-threaded. If `_byte_pair_merge_large` is wrong on its own — rather
/// than a victim of damage done elsewhere — this is where it shows, and the
/// finding is an upstream bug rather than a concurrency one.
#[test]
#[ignore = "needs the concurrent-process harness in this file's docs"]
fn tokenize_long_pieces_single_threaded() {
    hammer(1, 400);
}

/// Concurrent, matching the shape production runs: many workers, one shared
/// `CoreBPE` behind `retain`'s `OnceLock`.
///
/// `PROBE_THREADS` (default 16) and `PROBE_ITERS` (default 60) tune it from
/// the harness without a rebuild.
#[test]
#[ignore = "needs the concurrent-process harness in this file's docs"]
fn tokenize_long_pieces_concurrently() {
    let env = |k: &str, d: u64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    hammer(env("PROBE_THREADS", 16) as usize, env("PROBE_ITERS", 60));
}

/// Not ignored, so an ordinary `cargo test` keeps the path compiled and
/// exercised once: the long-piece text must reach `_byte_pair_merge_large`
/// and count the same from every thread.
#[test]
fn long_pieces_tokenize_consistently() {
    let text = long_pieces(7);
    assert!(
        text.split_whitespace().any(|w| w.len() >= 100),
        "the probe text must contain a piece over the 100-byte large-merge threshold"
    );
    let expected = memgardend::retain::token_count(&text);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| assert_eq!(memgardend::retain::token_count(&text), expected));
        }
    });
}
