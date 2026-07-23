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
//! Every lock above lives on an open fd. In wrap mode ([`acquire`]) that fd is
//! held by this `flock` process for the child's lifetime; in descriptor mode
//! ([`acquire_fd`]) the load-bearing fds (main + slots) are the *caller's*
//! inherited descriptors and the tokens ride those past our exit. Either way, on
//! any exit of the holder — including `SIGKILL` — the kernel closes the fds and
//! releases every token, with no stale-file cleanup. Same guarantee as exclusive
//! mode, extended to each slot.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::thread;
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

/// Like [`lock`], but for a *borrowed* (caller-owned) descriptor in descriptor
/// mode, where a failed `-w` must leave **nothing** held. The difference is the
/// `Until` path: [`lock`] backs the timeout with a helper thread blocking on a
/// `try_clone` of the fd, and on timeout it abandons that helper. For a fd we
/// *own* (wrap mode) that is safe — the clone's open file description dies with
/// us. For a caller's inherited fd it is not: the clone shares the caller's
/// OFD, so a late acquire by the abandoned helper would leak a lock onto the
/// caller's fd past our exit. Polling with non-blocking attempts keeps every
/// acquisition synchronous, so a timeout provably leaves the fd untouched.
fn lock_borrowed(file: &File, shared: bool, blocking: Blocking) -> Result<bool> {
    match blocking {
        // `Nonblock`/`Forever` never abandon a helper on a clone: the first is a
        // synchronous `try_lock`; the second blocks on this very fd and returns
        // only once held. Only `Until` needs the polling treatment.
        Blocking::Nonblock => try_lock(file, shared),
        Blocking::Forever => {
            blocking_lock(file, shared)?;
            Ok(true)
        }
        Blocking::Until(deadline) => loop {
            if try_lock(file, shared)? {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            thread::sleep((deadline - now).min(Duration::from_millis(20)));
        },
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
    std::fs::create_dir_all(&slots_dir)
        .with_context(|| format!("creating slots dir {}", slots_dir.display()))?;
    // Open all N slot files up front, then let the shared allocator lock K of
    // them; the rest drop (and release) at the end of this call.
    let slot_files: Vec<File> = (0..pool)
        .map(|i| open_slot(&slots_dir, i))
        .collect::<Result<_>>()?;
    let refs: Vec<&File> = slot_files.iter().collect();
    // Wrap mode owns these fds, so `lock` (helper-thread timeout) is safe and a
    // returned `None` releases everything by dropping `slot_files`/`main_file`.
    let Some(indices) = allocate_indices(&refs, tokens, blocking, lock)? else {
        return Ok(None);
    };
    drop(refs);
    drop(alloc);

    let mut guards = Vec::with_capacity(tokens + 1);
    guards.push(main_file);
    // Keep only the slot files we actually locked; dropping the others releases
    // them. Order within `guards` does not matter — each is held for its Drop.
    for (i, f) in slot_files.into_iter().enumerate() {
        if indices.contains(&i) {
            guards.push(f);
        }
    }
    Ok(Some(Held { guards }))
}

/// Releases the borrowed-fd locks (`main`, plus any slots) on drop — unless
/// `disarm`ed on the success path. Descriptor mode takes its load-bearing locks
/// on the *caller's* inherited fds, wrapped in `ManuallyDrop`, so nothing is
/// released implicitly on an early return. This guard makes cleanup uniform
/// across **every** non-success exit of [`acquire_fd`] — a refused `-n`/`-w`, a
/// `-w` timeout, and a genuine I/O `Err` propagated by `?` — each of which would
/// otherwise leak a residual main-shared (which blocks writers) and any swept
/// slots onto the caller's fds past our exit. Unlocking an fd we don't currently
/// hold is a harmless no-op, so the guard can cover every fd it *might* have
/// locked without tracking exactly which succeeded.
#[cfg(unix)]
struct BorrowedLockGuard {
    fds: Vec<i32>,
    armed: bool,
}

#[cfg(unix)]
impl BorrowedLockGuard {
    fn new(fds: Vec<i32>) -> Self {
        Self { fds, armed: true }
    }

    /// Extend coverage to more fds (the slot fds, once we may start locking them).
    fn cover(&mut self, fds: &[i32]) {
        self.fds.extend_from_slice(fds);
    }

    /// The success path calls this: the locks are meant to persist on the
    /// caller's fds, so drop must NOT release them.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for BorrowedLockGuard {
    fn drop(&mut self) {
        use std::mem::ManuallyDrop;
        use std::os::unix::io::FromRawFd as _;

        if !self.armed {
            return;
        }
        for &fd in &self.fds {
            // SAFETY: each fd is a caller-inherited descriptor; ManuallyDrop
            // keeps us from closing it — we only release any lock we hold on it.
            let f = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
            let _ = f.unlock();
        }
    }
}

/// Descriptor-mode weighted acquire — the fd analogue of [`acquire`].
///
/// The caller (a shell) has already opened the pool's files as *inherited* fds
/// that outlive us: `main_fd` on the lockfile itself, and — for a reader —
/// `slot_fds` on the `N` slot files (`<lockfile>.slots/00`..) in slot order.
/// We take the load-bearing locks (the shared/exclusive main lock, plus `K`
/// slot locks) on those inherited fds; because `flock(2)` locks live on the
/// open file description, they persist after we exit, held by the caller's
/// still-open fds exactly like plain descriptor mode. The transient
/// coordination locks (`.intent`, `.alloc`) are taken on *our own* fds and
/// release when we exit — they are never meant to outlive acquisition.
///
/// Returns the slot indices we locked (so the caller can report/track them), or
/// `Ok(None)` under `-n`/`-w`. `slot_fds` must be empty for a writer (`K==N`)
/// and length `pool` for a reader.
#[cfg(unix)]
pub fn acquire_fd(
    lockfile: &Path,
    main_fd: i32,
    slot_fds: &[i32],
    tokens: usize,
    pool: usize,
    blocking: Blocking,
    verbose: bool,
) -> Result<Option<Vec<usize>>> {
    use std::mem::ManuallyDrop;
    use std::os::unix::io::FromRawFd as _;

    debug_assert!(tokens >= 1 && tokens <= pool);

    let intent = open_lockfile(&sibling(lockfile, ".intent")).context("opening intent gate")?;

    // SAFETY: `main_fd` is a descriptor the caller passed us. ManuallyDrop keeps
    // us from `close(2)`-ing the caller's fd; we only lock it. The lock lives on
    // the shared OFD, so it stays held after we exit.
    let main_file = ManuallyDrop::new(unsafe { File::from_raw_fd(main_fd) });

    if tokens == pool {
        // Writer: hold the entry gate exclusive while waiting (blocks new
        // readers at the gate so the pool drains and we are not starved), then
        // take the main lock exclusive on the inherited fd.
        if !lock(&intent, false, blocking)? {
            return Ok(None);
        }
        if verbose {
            eprintln!("flock: holding writer gate, waiting for the pool to drain…");
        }
        // The guard releases main on any early/error exit; disarmed only on
        // success. `main_file` is the caller's fd, so `lock_borrowed` polls the
        // `-w` timeout rather than abandoning a helper on a clone that shares
        // the caller's OFD.
        let mut guard = BorrowedLockGuard::new(vec![main_fd]);
        if !lock_borrowed(&main_file, false, blocking)? {
            return Ok(None);
        }
        // Release the entry gate at acquisition — unlike wrap mode, where the
        // writer holds it for the child's whole run. This weakens nothing: once
        // the main lock is held exclusive, any new reader blocks on the main
        // lock itself (it can only take main *shared*), so dropping the gate
        // does not let a reader slip in. The gate exists purely to win the race
        // for main against a reader *stream*; that race is over once we hold
        // main. (`intent` is our own fd and would release on exit regardless;
        // dropping it here makes the intent explicit.)
        drop(intent);
        guard.disarm();
        return Ok(Some(Vec::new()));
    }

    // Reader: brief pass through the exclusive entry gate, main shared (interop),
    // release the gate, then allocate K of the inherited slot fds. Descriptor
    // mode is *all-or-nothing*: the load-bearing locks sit on the caller's fds
    // (ManuallyDrop releases nothing on its own), so `BorrowedLockGuard` releases
    // them on every non-success exit below — refusal, `-w` timeout, or a genuine
    // I/O `Err` propagated by `?` — leaving a residual main-shared (which would
    // block writers) plus any swept slots only when we actually succeed.
    if !lock(&intent, false, blocking)? {
        return Ok(None);
    }
    // `main_file` is the caller's fd — poll (never abandon a helper on a clone
    // of it). A `false` return means main was never acquired: nothing held.
    if !lock_borrowed(&main_file, true, blocking)? {
        return Ok(None);
    }
    drop(intent);

    // Main-shared is now held on the caller's fd; arm the guard before any
    // fallible step so an `Err` from the `?`s below still releases it.
    let mut guard = BorrowedLockGuard::new(vec![main_fd]);

    let alloc = open_lockfile(&sibling(lockfile, ".alloc")).context("opening allocator lock")?;
    if !lock(&alloc, false, blocking)? {
        return Ok(None);
    }

    // SAFETY: each `slot_fds` entry is an inherited descriptor on a slot file.
    // ManuallyDrop keeps us from closing the caller's fds; the K locks we take
    // ride the caller's OFDs past our exit.
    let slot_files: Vec<ManuallyDrop<File>> = slot_fds
        .iter()
        .map(|&fd| ManuallyDrop::new(unsafe { File::from_raw_fd(fd) }))
        .collect();
    // Extend guard coverage to the slots before we may lock any of them, so an
    // `Err` or `None` from `allocate_indices` releases the swept slots too.
    guard.cover(slot_fds);
    let refs: Vec<&File> = slot_files.iter().map(|f| &**f).collect();
    let Some(indices) = allocate_indices(&refs, tokens, blocking, lock_borrowed)? else {
        return Ok(None);
    };

    drop(alloc);
    guard.disarm();
    Ok(Some(indices))
}

#[cfg(not(unix))]
pub fn acquire_fd(
    _lockfile: &Path,
    _main_fd: i32,
    _slot_fds: &[i32],
    _tokens: usize,
    _pool: usize,
    _blocking: Blocking,
    _verbose: bool,
) -> Result<Option<Vec<usize>>> {
    anyhow::bail!("descriptor lock mode is only supported on unix");
}

/// Grab `tokens` distinct free slots out of `slots` (length `pool`), holding the
/// allocator lock. Returns the indices locked, with those locks held on the
/// passed `File`s. Shared by wrap ([`acquire`]) and descriptor ([`acquire_fd`])
/// mode — the only differences are where the slot `File`s come from and which
/// blocking primitive is safe on them, so the caller passes `lock_one` (`lock`
/// for wrap's owned fds, `lock_borrowed` for descriptor mode's caller fds). On a
/// `None` return the partial locks stay held on the passed `File`s; the caller
/// releases them (wrap by dropping the owned `File`s, descriptor by unlocking).
///
/// Greedy: sweep non-blocking for whatever is free, and if that isn't enough,
/// block on the lowest not-yet-held slot until a running reader frees it, then
/// sweep again. Because the caller holds the allocator lock throughout, no other
/// allocator can snatch a freed slot between our sweeps — a slot we saw free
/// stays free until we take it — so this makes guaranteed forward progress with
/// no busy-wait and no two-allocator deadlock.
fn allocate_indices(
    slots: &[&File],
    tokens: usize,
    blocking: Blocking,
    lock_one: impl Fn(&File, bool, Blocking) -> Result<bool>,
) -> Result<Option<Vec<usize>>> {
    let pool = slots.len();
    let mut held: Vec<usize> = Vec::with_capacity(tokens);
    loop {
        // Opportunistic non-blocking sweep for any free slots.
        for (i, slot) in slots.iter().enumerate() {
            if held.len() == tokens {
                break;
            }
            if held.contains(&i) {
                continue;
            }
            if try_lock(slot, false)? {
                held.push(i);
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
            .find(|i| !held.contains(i))
            .expect("tokens <= pool guarantees an un-held index exists");
        if !lock_one(slots[next], false, blocking)? {
            return Ok(None); // -w deadline hit
        }
        held.push(next);
    }
    Ok(Some(held))
}
