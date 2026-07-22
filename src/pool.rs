//! Weighted reader/writer acquisition — the `--tokens K --pool N` mode.
//!
//! This layers a *counting* semaphore on top of the exclusive lock the tool
//! already provides, without giving up any of its guarantees (atomicity,
//! death-safety, no stale-file GC). A caller asks for `K` tokens out of a pool
//! of `N`; `K < N` is a shared *reader* (several fit side by side, up to `N`
//! tokens total), and `K == N` is a *writer* that drains the whole pool and is
//! therefore exclusive. The point is to serialize a shared machine by *load*
//! instead of by "any command at all".
//!
//! ## Files, all derived from the one lockfile path the caller passes
//!
//! - **`<lockfile>`** — the *interop + resource* lock. A reader holds it
//!   `shared` for its whole run; a writer holds it `exclusive`. This is the
//!   single boundary a *legacy* plain-`flock <lockfile>` holder (which knows
//!   nothing of tokens) still respects: its exclusive lock blocks every token
//!   holder, and every token holder's shared lock blocks it. Interop is
//!   entirely carried here, so old and new binaries stay mutually correct on
//!   the same lockfile.
//! - **`<lockfile>.intent`** — the *entry gate*, an exclusive mutex taken
//!   briefly by a reader (to pass through) and held for wait+run by a writer.
//!   A writer holding it stops *new* readers before they can take the resource
//!   lock, so the resource drains and the writer is not starved by a reader
//!   stream. (See the note on why this gate is exclusive, not shared, below.)
//! - **`<lockfile>.alloc`** — a brief internal *allocator* mutex held only
//!   while a reader hunts for its `K` free slots, so slot allocation is atomic
//!   and two readers can never deadlock each grabbing a disjoint partial set.
//! - **`<lockfile>.slots/NN`** — the pool itself: `N` slot files, each an
//!   exclusive `flock`. A `K`-token reader holds `K` distinct slot fds for its
//!   run; the count of taken slots *is* the pool's occupancy. A writer takes no
//!   slots — its exclusive hold on the resource lock already excludes readers.
//!
//! ## Why the gate is exclusive, not shared
//!
//! `flock(2)` gives no writer preference: a continuous stream of *shared*
//! lockers can keep a lock perpetually in shared state and starve an
//! *exclusive* waiter forever. So a gate that readers took *shared* would not,
//! on its own, guarantee the writer ever acquires. Making the gate a plain
//! exclusive mutex that every entrant holds *briefly* removes the pathology:
//! the gate is never continuously held, so the writer wins it in bounded time,
//! and while the writer holds it no new reader can pass. Readers still run
//! concurrently — they release the gate before running — so nothing is lost.
//!
//! ## Death-safety
//!
//! Every lock above lives on an open fd held by this `flock` process for the
//! child's lifetime. On any exit — including `SIGKILL` — the kernel closes the
//! fds and releases every token, with no stale-file cleanup. Same guarantee as
//! exclusive mode, extended to each slot.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::{blocking_lock, lock_with_timeout, open_lockfile, try_lock};

/// Blocking policy for a single acquisition, derived once from `-n` / `-w`.
#[derive(Clone, Copy)]
pub enum Blocking {
    /// Block indefinitely (the default).
    Forever,
    /// One non-blocking attempt, then give up (`-n`).
    Nonblock,
    /// Keep trying until this instant, then give up (`-w SECONDS`).
    Until(Instant),
}

impl Blocking {
    pub fn from_opts(nonblock: bool, timeout: Option<f64>) -> Self {
        if nonblock {
            Blocking::Nonblock
        } else if let Some(secs) = timeout {
            Blocking::Until(Instant::now() + Duration::from_secs_f64(secs.max(0.0)))
        } else {
            Blocking::Forever
        }
    }
}

/// Take one lock under the blocking policy. `Ok(true)` = held, `Ok(false)` =
/// could not acquire under `-n`/`-w`, `Err` = an unexpected lock error.
fn lock(file: &File, shared: bool, blocking: Blocking) -> Result<bool> {
    match blocking {
        Blocking::Nonblock => try_lock(file, shared),
        Blocking::Forever => {
            blocking_lock(file, shared)?;
            Ok(true)
        }
        Blocking::Until(deadline) => {
            let now = Instant::now();
            if deadline <= now {
                // Deadline already passed — one last non-blocking attempt.
                try_lock(file, shared)
            } else {
                lock_with_timeout(file, shared, deadline - now)
            }
        }
    }
}

/// The locks held for the child's lifetime. Dropping this releases them (the
/// kernel would anyway, on death); the order within does not matter.
pub struct Held {
    // Held only for its `Drop`: closing these fds releases the tokens. Never
    // read, by design — the value's lifetime *is* the effect.
    #[allow(dead_code)]
    guards: Vec<File>,
}

/// Derive `<lockfile><suffix>` (a sibling path next to the lockfile).
fn sibling(lockfile: &Path, suffix: &str) -> PathBuf {
    let mut name = lockfile.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    lockfile.with_file_name(name)
}

fn open_slot(slots_dir: &Path, i: usize) -> Result<File> {
    let path = slots_dir.join(format!("{i:02}"));
    open_lockfile(&path).with_context(|| format!("opening slot {}", path.display()))
}

/// Acquire `K` of `N` tokens on `lockfile`, or return `Ok(None)` when `-n`/`-w`
/// cannot. `main_file` is the already-opened resource lockfile.
///
/// `K == N` is the writer/exclusive path; `K < N` is a reader.
pub fn acquire(
    lockfile: &Path,
    main_file: File,
    tokens: usize,
    pool: usize,
    blocking: Blocking,
    verbose: bool,
) -> Result<Option<Held>> {
    debug_assert!(tokens >= 1 && tokens <= pool);

    let intent = open_lockfile(&sibling(lockfile, ".intent")).context("opening intent gate")?;

    if tokens == pool {
        // Writer: hold the entry gate exclusive for wait+run (blocks new
        // readers at the gate), then take the resource lock exclusive (drains
        // current readers / legacy holders). No slots — the exclusive resource
        // lock already excludes every reader.
        if !lock(&intent, false, blocking)? {
            return Ok(None);
        }
        if verbose {
            eprintln!("flock: holding writer gate, waiting for the pool to drain…");
        }
        if !lock(&main_file, false, blocking)? {
            return Ok(None);
        }
        return Ok(Some(Held {
            guards: vec![intent, main_file],
        }));
    }

    // Reader: pass briefly through the exclusive entry gate (blocked only while
    // a writer holds it), take the resource lock shared (interop), release the
    // gate, then allocate K slots under the allocator mutex.
    if !lock(&intent, false, blocking)? {
        return Ok(None);
    }
    if !lock(&main_file, true, blocking)? {
        return Ok(None);
    }
    drop(intent); // entry gate is pass-through for readers

    let alloc = open_lockfile(&sibling(lockfile, ".alloc")).context("opening allocator lock")?;
    if !lock(&alloc, false, blocking)? {
        return Ok(None);
    }

    let slots_dir = sibling(lockfile, ".slots");
    let Some(slots) = allocate(&slots_dir, tokens, pool, blocking)? else {
        return Ok(None);
    };
    drop(alloc);

    let mut guards = Vec::with_capacity(tokens + 1);
    guards.push(main_file);
    guards.extend(slots);
    Ok(Some(Held { guards }))
}

/// Grab `tokens` distinct free slots out of `pool`, holding the allocator lock.
///
/// Greedy: sweep the pool non-blocking for whatever is free, and if that isn't
/// enough, block on the lowest not-yet-held slot until a running reader frees
/// it, then sweep again. Because we hold the allocator lock throughout, no
/// other allocator can snatch a freed slot between our sweeps — a slot we saw
/// free stays free until we take it — so this makes guaranteed forward progress
/// with no busy-wait and no two-allocator deadlock.
fn allocate(
    slots_dir: &Path,
    tokens: usize,
    pool: usize,
    blocking: Blocking,
) -> Result<Option<Vec<File>>> {
    std::fs::create_dir_all(slots_dir)
        .with_context(|| format!("creating slots dir {}", slots_dir.display()))?;

    let mut held: Vec<(usize, File)> = Vec::with_capacity(tokens);
    loop {
        // Opportunistic non-blocking sweep for any free slots.
        for i in 0..pool {
            if held.len() == tokens {
                break;
            }
            if held.iter().any(|(j, _)| *j == i) {
                continue;
            }
            let f = open_slot(slots_dir, i)?;
            if try_lock(&f, false)? {
                held.push((i, f));
            }
        }
        if held.len() == tokens {
            break;
        }
        if let Blocking::Nonblock = blocking {
            return Ok(None); // pool exhausted right now, and we were told not to wait
        }
        // Not enough free yet: block on the lowest slot we don't hold. It is
        // held by a running reader that will release it, and no other allocator
        // can be running (we hold the allocator lock), so this always unblocks.
        let next = (0..pool)
            .find(|i| !held.iter().any(|(j, _)| j == i))
            .expect("tokens <= pool guarantees an un-held index exists");
        let f = open_slot(slots_dir, next)?;
        if !lock(&f, false, blocking)? {
            return Ok(None); // -w deadline hit
        }
        held.push((next, f));
    }
    Ok(Some(held.into_iter().map(|(_, f)| f).collect()))
}
