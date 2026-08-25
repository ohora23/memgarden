# AC-7 — the audit, and the half of it that fails

AC-7 asks two things of the finished project: that **every PR followed the
agreed template** (PRD ID + verification evidence), and that **`cargo test`
passes**. Both were checked on 2026-08-26 against the 27 merged PRs and the
workspace suite. Neither holds as stated, and the reasons are different.

## Half one — the template was adopted at #14, not at #1

`gh pr view` over all 27 merged PRs, checking for the headings
`.github/pull_request_template.md` defines:

| PRs | `## Summary` | `## PRD Items` | `## Verification` | verdict |
|---|---|---|---|---|
| **#14–#27** (14) | 14/14 | 14/14 | 14/14 | **compliant** |
| **#1–#13** (13) | 0/13 | 0/13 | 4/13 | **not compliant** |

`## Changes` is absent from #22, #24, #26 and #27, all of which are single-
concern PRs whose summary is the change list. That is the only deviation
inside the compliant range and it is not counted against them.

**The gap is one of form, not of substance.** Of the thirteen:

* **12 of 13 carry verification evidence in prose** — measured numbers, test
  counts, or a before/after table. #9 is the clearest case: it reports the
  `PRAGMA journal_mode` A/B (`memory` vs `wal`) that found the harness had
  never run under WAL, with the failing-pair measurement beside it. No
  heading; the evidence is there.
* **7 of 13 name a PRD ID** in the body text (`AX-2`, `CE-7`, `AC-4`).
* **#5 is the one PR with neither** — `actions/checkout` v4 → v5, a CI action
  bump whose body explains why it was opened as a PR at all.

### These bodies were not edited to make this table pass

They could have been. `gh pr edit` will rewrite a merged PR's body, and
thirteen edits would have turned this criterion green in about a minute.

That is fabricating a record of a process that did not happen, and it is the
opposite of what the rest of this project's evidence is for. The same rule
retired the first AC-1 measurement, the 64× semantic-link claim, the CPU-3
conclusion and the gold-harness retraction: **when the record disagrees with
the claim, the claim moves.** So the claim moves here.

**Proposed amendment to AC-7:** the template was adopted at #14 and held for
every PR from #14 to #27 without exception. #1–#13 predate its adoption; they
carry PRD IDs and verification evidence in prose but not under the template's
headings. AC-7's template clause is satisfied *from #14 onward* and is
recorded as not satisfied for #1–#13.

**Signed by the user on 2026-08-26.** The template clause of AC-7 is recorded
as satisfied on those terms. It needed a signature the way AC-1 did; it is not
a thing the author of the audit gets to wave through.

## Half two — `cargo test` does not pass, and it is not this project's fault

`cargo test --workspace` dies intermittently with **SIGSEGV in
`-p memgardend --lib`**: 2 of the first 4 runs on 2026-08-26. This is the
"heap corruption under test load" that has been open since 2026-08-09 and that
ASAN could never reproduce.

It is not a MemGarden defect. `/var/crash/` holds **three kernel crash
dumps**, saved by kdump at each of the abrupt machine deaths recorded through
August and never opened:

| dump | CPU | task | fault |
|---|---|---|---|
| `202608200204` | **3** | `swapper/3` | page fault at `00002001747d2688` in `sched_ttwu_pending` |
| `202608210441` | **3** | `tokio-rt-worker` | `Oops: Bad pagetable` |
| `202608260115` | **3** | `migrate::import` | `irq_fpu_usable` WARN → `scheduling while atomic` → page fault in `futex_wait_setup` |

Three panics, three unrelated kernel paths, **all on CPU 3**. The 08-20 dump
is the decisive one: the faulting task is `swapper/3`, the idle task. No
userspace code is running at that point, so no userspace code can be the
cause.

The 08-26 dump also explains the shape of the userspace symptom. The
userspace operation is an ordinary `openat(O_TRUNC)`; ext4 computes the
superblock CRC32C, the hardware-accelerated `crc32c` asks for the FPU,
`irq_fpu_usable()` warns, and the kernel then schedules while atomic — after
which it can corrupt anything, including the test process. **The SIGSEGV is
plausibly a consequence of the kernel fault, not a cause of anything**, which
is why ASAN found nothing: there was no userspace heap bug to find.

The workspace suite creates and truncates a large number of temporary
database files — every `Db::open_memory` has opened a throwaway file since
PR #9 — which is why only whole-workspace load reproduces it. The
`memgardend` test binary alone, run 8 times under gdb, never crashed.

### The CPU-3 conclusion, withdrawn once, has better evidence now

It was withdrawn because its basis was a userspace reproduction rate (12/40,
then 0 in 160 retries) — a noisy instrument whose swing invalidated most of
the bisection it was used for. Kernel panics are a cleaner one, and they read
3 for 3.

`cpu3` is physical core 3; its SMT sibling is `cpu11`. AMD Ryzen 7 9800X3D,
MSI MAG B850M MORTAR WIFI, BIOS 1.A40. All three panics were on kernel
`7.0.0-29-generic`; the machine now runs `7.0.0-30-generic`, which is a
confound any further measurement has to control for.

**AC-7's `cargo test` clause cannot be honestly ticked until this is
resolved**, and resolving it is a hardware question, not a code one.

### What changed at 01:17 on 2026-08-26, and what it does not prove

`linux-image-7.0.0-30-generic` was installed on **2026-08-25 00:38** and not
booted. The machine kept running `-29` for another day — through the MX-3 run
and the retain fix — until the 01:15 panic forced the reboot that first
brought `-30` up.

Every panic and every reproduction of the userspace symptom is therefore on
`-29`:

| | kernel | `cargo test --workspace` SIGSEGV |
|---|---|---|
| before the reboot | `-29` | **2 of 4** |
| after the reboot | `-30` | **0 of 55** (10 + a 45-run soak, no kernel WARNING either) |

Fisher's exact on those counts is p ≈ 0.008, and it still does not identify a
cause, because the reboot changed three things at once: the kernel version,
the uptime, and the whole of physical memory.

**Uptime is the reason the soak cannot settle it.** The three panics fired at
uptimes of 1.7 h, 13 h and 31 h. This is an event measured in hours, and 55
clean runs inside the first hour of a fresh boot is not evidence against a
fault that takes hours to surface.

So the CPU-3 experiment — taking `cpu3`/`cpu11` offline and re-soaking — was
**designed and then dropped as a measurement**: with the control arm already
at zero there is nothing for the treatment arm to subtract from.

The user then took `cpu3`/`cpu11` offline anyway at 02:07, **as a precaution
rather than as an experiment** (`nproc` 16 → 14). That is a different and
defensible reason: if the hypothesis is right, the machine stops panicking.
It changes what the coming days can tell us, asymmetrically:

* **quiet past 31 h → still undecided.** A kernel fix and a removed core are
  not separable from a quiet machine.
* **a panic → the CPU-3 hypothesis is refuted.** The suspect core was not
  running, so it cannot be the cause, and kdump writes another dump either
  way.

The offline state **does not survive a reboot**, which matters because a
memtest86+ run needs one; `/etc/tmpfiles.d/` will hold it across boots if that
is wanted.

What discriminates costs nothing: **use the machine.** If `-30` passes 31
hours of uptime quietly, `-29` was the fault. If it panics, kdump writes
another dump and the CPU number in it decides. Three dumps sat unopened in
`/var/crash/` through August; that they are worth opening is the durable
finding here.

## Status

| clause | verdict |
|---|---|
| every PR follows the template | **yes, as amended** — holds #14–#27 without exception; #1–#13 predate adoption. User-signed 2026-08-26. |
| `cargo test` passes | **not decidable yet** — 2 of 4 runs died on kernel `-29`; 55 of 55 pass on `-30`, but only at low uptime. Traced to a kernel fault on CPU 3, not to this codebase. Closes when `-30` has run quietly past 31 h of uptime. |
