//! End-to-end tests for weighted mode (`--tokens K --pool N`).
//!
//! Like `integration.rs`, these spawn the real built `flock` binary — the
//! behaviour that matters is what independent processes observe. The proofs:
//!
//! - `shared_tokens_run_concurrently`: two readers that fit the pool actually
//!   overlap (the pool does not serialize them).
//! - `pool_exhaustion_blocks`: a reader that would overflow the pool is refused
//!   (`-n`), while one that still fits is admitted.
//! - `writer_waits_for_drain_then_wins_over_readers`: a full-pool writer under a
//!   continuous reader stream acquires after the in-flight readers drain — it is
//!   not starved (writer preference), and while it waits new readers are blocked.
//! - `writer_intent_blocks_new_readers`: while a writer holds the pool, a new
//!   reader is refused even though, token-count-wise, it would fit.
//! - `sigkill_frees_all_tokens`: SIGKILLing a reader releases every slot it
//!   held, so the freed capacity is immediately reusable.
//! - `legacy_exclusive_blocks_tokens` / `tokens_block_legacy_exclusive`: a plain
//!   exclusive holder (the pre-tokens binary) and token holders are mutually
//!   exclusive through the main lockfile — the interop guarantee.

use std::fs;
use std::path::Path;
use std::process::{Child, Command as ProcCommand};
use std::thread;
use std::time::{Duration, Instant};

use assert_fs::TempDir;
use assert_fs::prelude::*;
use assert2::check;

fn flock() -> ProcCommand {
    ProcCommand::new(env!("CARGO_BIN_EXE_flock"))
}

/// True if an exclusive lock on the *main* lockfile can be taken right now.
/// A token reader holds it shared and a writer holds it exclusive, so this is
/// false whenever any token holder is active.
fn main_lock_free(path: &Path) -> bool {
    flock()
        .arg("-n")
        .arg(path)
        .args(["true"])
        .status()
        .unwrap()
        .success()
}

fn wait_until(path: &Path, timeout: Duration, mut pred: impl FnMut(&Path) -> bool) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred(path) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "condition not met within {timeout:?} for {}",
        path.display()
    );
}

/// Spawn a token holder that holds `tokens/pool` for a `sleep`, in its own
/// process group so the whole tree can be SIGKILLed together. Killed on drop.
struct Reader {
    /// `None` once killed — see [`kill_group`] on why a reaped child must not
    /// be signalled a second time.
    child: Option<Child>,
}

impl Reader {
    fn spawn(lockfile: &Path, tokens: usize, pool: usize, sleep_secs: &str) -> Self {
        let mut cmd = flock();
        cmd.args(["--tokens", &tokens.to_string(), "--pool", &pool.to_string()])
            .arg(lockfile)
            .args(["sleep", sleep_secs]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let child = cmd.spawn().expect("spawn reader");
        let reader = Reader { child: Some(child) };
        // Wait until it actually holds (main lock reads as not-free).
        wait_until(lockfile, Duration::from_secs(5), |p| !main_lock_free(p));
        reader
    }

    fn kill(&mut self) {
        if let Some(child) = self.child.take() {
            kill_group(child);
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.kill();
    }
}

/// SIGKILL a holder's whole process group, then reap it.
///
/// `killpg(2)` directly rather than shelling out to `kill(1)`: which `kill`
/// gets exec'd is the platform's, and they do not agree on `-KILL -<pgid>`.
/// BSD's (macOS) kills the group; procps-ng's (Debian/Ubuntu, so CI and any
/// Linux container) exits 1 there and left holders running — those runs stalled
/// for the holder's whole `sleep` instead of killing it. The syscall has no
/// argument parsing and no `PATH` to get wrong.
///
/// Takes the `Child` by value because reaping it frees its pid for reuse: a
/// second `killpg` on the same number could land on an unrelated process group.
fn kill_group(mut child: Child) {
    #[cfg(unix)]
    // SAFETY: killpg takes no pointers, so it cannot violate memory safety. A
    // pgid that is already gone just returns ESRCH, which we ignore like any
    // other error here.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------

/// Two readers that fit the pool must overlap. Each appends `S`, sleeps, then
/// `E`; concurrency shows up as two `S` before any `E`.
#[test]
fn shared_tokens_run_concurrently() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    let log = dir.child("log");
    fs::write(log.path(), "").unwrap();

    let script = format!(
        "printf 'S\\n' >> {l}; sleep 0.3; printf 'E\\n' >> {l}",
        l = log.path().display()
    );
    let spawn = || {
        flock()
            .args(["--tokens", "4", "--pool", "16"])
            .arg(lockfile.path())
            .args(["sh", "-c", &script])
            .spawn()
            .unwrap()
    };
    let mut a = spawn();
    let mut b = spawn();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());

    let contents = fs::read_to_string(log.path()).unwrap();
    let markers: Vec<&str> = contents.lines().collect();
    check!(markers.len() == 4, "log = {markers:?}");
    check!(
        markers[0] == "S" && markers[1] == "S",
        "two readers should overlap (expected S,S,…); log = {markers:?}"
    );
}

/// A reader that would overflow the pool is refused with `-n`; one that still
/// fits is admitted.
#[test]
fn pool_exhaustion_blocks() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");

    // Hold 3 of a 4-slot pool.
    let _holder = Reader::spawn(lockfile.path(), 3, 4, "30");

    // A 2-token reader would need 2 of the 1 remaining slot → refused fast.
    let start = Instant::now();
    let overflow = flock()
        .args(["--tokens", "2", "--pool", "4", "-n"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        overflow.code() == Some(1),
        "overflowing reader must be refused"
    );
    check!(
        start.elapsed() < Duration::from_secs(2),
        "-n did not fail fast"
    );

    // A 1-token reader fits the last slot → admitted.
    let fits = flock()
        .args(["--tokens", "1", "--pool", "4", "-n"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        fits.success(),
        "a reader that fits the last slot must be admitted"
    );
}

/// A full-pool writer under a continuous reader stream acquires after the
/// in-flight readers drain — not never. The reader stream is a shell loop that
/// keeps launching readers until a stop file appears; once the writer holds the
/// entry gate the loop's next reader blocks, so the pool drains and the writer
/// wins within a bounded time.
#[test]
fn writer_waits_for_drain_then_wins_over_readers() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    let stop = dir.child("stop");
    let bin = env!("CARGO_BIN_EXE_flock");

    let loop_script = format!(
        "while [ ! -f {stop:?} ]; do {bin:?} --tokens 4 --pool 16 {lock:?} sleep 0.15; done",
        stop = stop.path(),
        lock = lockfile.path(),
    );
    let stream = {
        let mut cmd = ProcCommand::new("sh");
        cmd.args(["-c", &loop_script]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        cmd.spawn().unwrap()
    };

    // Let the stream get going.
    wait_until(lockfile.path(), Duration::from_secs(5), |p| {
        !main_lock_free(p)
    });

    // Full-pool writer with a generous timeout; it must acquire despite the
    // stream. Time it to prove it wasn't starved.
    let start = Instant::now();
    let writer = flock()
        .args(["--tokens", "16", "--pool", "16", "-w", "10"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    let elapsed = start.elapsed();

    // Stop the stream and reap it.
    fs::write(stop.path(), "go").unwrap();
    kill_group(stream);

    check!(
        writer.success(),
        "writer was starved by the reader stream (exited {writer:?})"
    );
    check!(
        elapsed < Duration::from_secs(10),
        "writer did not acquire within the timeout — starvation"
    );
}

/// While a writer holds the whole pool, a new reader is refused even though its
/// token count alone would fit an empty pool — the writer's exclusive hold, via
/// the entry gate and main lock, blocks it.
#[test]
fn writer_intent_blocks_new_readers() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");

    // Writer holds 16/16.
    let _writer = Reader::spawn(lockfile.path(), 16, 16, "30");

    let reader = flock()
        .args(["--tokens", "1", "--pool", "16", "-n"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        reader.code() == Some(1),
        "a new reader must be blocked while a writer holds the pool"
    );
}

/// SIGKILLing a reader frees every slot it held. After killing an 8-token
/// holder, two fresh 8-token holders must fit concurrently (8+8 = the whole
/// 16-pool) — only possible if the killed holder's 8 slots were released.
#[cfg(unix)]
#[test]
fn sigkill_frees_all_tokens() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");

    let mut victim = Reader::spawn(lockfile.path(), 8, 16, "30");
    victim.kill();

    // The main lock must read free again promptly (shared hold released).
    wait_until(lockfile.path(), Duration::from_secs(5), main_lock_free);

    // Now prove the *slots* freed: two 8-token readers fit side by side.
    let _a = Reader::spawn(lockfile.path(), 8, 16, "3");
    let start = Instant::now();
    let b = flock()
        .args(["--tokens", "8", "--pool", "16", "-w", "3"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        b.success(),
        "second 8-token holder could not acquire — killed reader's slots leaked"
    );
    check!(
        start.elapsed() < Duration::from_secs(3),
        "slot reuse was not prompt"
    );
}

/// Interop: a legacy plain-exclusive holder (`flock <lockfile>`, no tokens)
/// blocks a token reader — they share the main lockfile.
#[test]
fn legacy_exclusive_blocks_tokens() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");

    // Legacy holder: plain exclusive, no --tokens.
    let mut legacy = flock()
        .arg(lockfile.path())
        .args(["sleep", "30"])
        .spawn()
        .unwrap();
    wait_until(lockfile.path(), Duration::from_secs(5), |p| {
        !main_lock_free(p)
    });

    let reader = flock()
        .args(["--tokens", "1", "--pool", "16", "-n"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        reader.code() == Some(1),
        "a token reader must be blocked by a legacy exclusive holder"
    );

    let _ = legacy.kill();
    let _ = legacy.wait();
}

/// Interop, the other direction: a token reader blocks a legacy plain-exclusive
/// acquirer.
#[test]
fn tokens_block_legacy_exclusive() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");

    let _reader = Reader::spawn(lockfile.path(), 1, 16, "30");

    let legacy = flock()
        .arg("-n")
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        legacy.code() == Some(1),
        "a legacy exclusive acquirer must be blocked by a token reader"
    );
}
