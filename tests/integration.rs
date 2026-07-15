//! End-to-end tests that prove the lock is a *real* kernel mutex.
//!
//! These spawn the actual built `flock` binary, because the whole point of the
//! tool is the behaviour observed by independent processes — not anything we
//! could assert about in-process state. The high-value proofs are:
//!
//! - `mutual_exclusion_*`: N processes racing to non-atomically increment a
//!   shared counter never lose an update (the exact property the socket lock's
//!   TOCTOU violated).
//! - `sigkill_releases_lock`: a `SIGKILL`ed holder's lock is released by the
//!   kernel immediately, so a waiter acquires.

use std::fs;
use std::path::Path;
use std::process::Command as ProcCommand;
use std::thread;
use std::time::{Duration, Instant};

use assert_fs::TempDir;
use assert_fs::prelude::*;
use assert2::check;

/// Path to the freshly built `flock` binary under test.
fn flock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_flock")
}

/// A `std::process::Command` for the binary under test.
fn flock_cmd() -> ProcCommand {
    ProcCommand::new(flock_bin())
}

// ---------------------------------------------------------------------------
// Mutual exclusion under contention — the load-bearing proof.
// ---------------------------------------------------------------------------

/// N processes each: read counter file -> sleep (widen the race) -> write
/// counter+1, all serialized by the same lock. With a real mutex the final
/// value is exactly N; any lost update (two holders inside the critical
/// section at once) leaves it below N.
fn run_counter_round(lockfile: &Path, counter: &Path, workers: usize) {
    fs::write(counter, "0").unwrap();

    // A read-modify-write with a sleep between read and write. Unguarded, this
    // is a textbook lost-update race; the lock must serialize it.
    let script = format!(
        "n=$(cat {c}); sleep 0.02; echo $((n + 1)) > {c}",
        c = counter.display()
    );

    let mut children = Vec::with_capacity(workers);
    for _ in 0..workers {
        let child = flock_cmd()
            .arg(lockfile)
            .args(["sh", "-c", &script])
            .spawn()
            .expect("spawn flock worker");
        children.push(child);
    }
    for mut child in children {
        let status = child.wait().expect("wait for worker");
        check!(status.success(), "worker exited non-zero: {status:?}");
    }

    let final_value: usize = fs::read_to_string(counter).unwrap().trim().parse().unwrap();
    check!(
        final_value == workers,
        "lost update: counter={final_value} but {workers} workers ran (lock let holders overlap)"
    );
}

#[test]
fn mutual_exclusion_no_lost_updates() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    let counter = dir.child("counter");

    // Repeat the whole contended round several times: a mutex bug is often
    // probabilistic, so one clean round proves little.
    for round in 0..8 {
        run_counter_round(lockfile.path(), counter.path(), 16);
        // Between rounds the lock must be fully released (all fds closed).
        check!(
            is_free(lockfile.path()),
            "lock not free after round {round}"
        );
    }
}

/// Second, orthogonal proof: interleaving detection. Each holder appends a
/// START marker, sleeps, then an END marker. Under a real mutex the markers
/// must be perfectly nested (S,E,S,E,…); an overlap shows up as S,S.
#[test]
fn mutual_exclusion_no_interleave() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    let log = dir.child("log");
    fs::write(log.path(), "").unwrap();

    let workers = 12;
    let script = format!(
        "printf 'S\\n' >> {l}; sleep 0.02; printf 'E\\n' >> {l}",
        l = log.path().display()
    );

    let mut children = Vec::with_capacity(workers);
    for _ in 0..workers {
        children.push(
            flock_cmd()
                .arg(lockfile.path())
                .args(["sh", "-c", &script])
                .spawn()
                .unwrap(),
        );
    }
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }

    let contents = fs::read_to_string(log.path()).unwrap();
    let markers: Vec<&str> = contents.lines().collect();
    check!(markers.len() == workers * 2);
    // Strict alternation S,E,S,E,… proves no two critical sections overlapped.
    for (i, m) in markers.iter().enumerate() {
        let expected = if i % 2 == 0 { "S" } else { "E" };
        check!(
            *m == expected,
            "interleave at position {i}: log = {markers:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Death-release — the kernel frees the lock when the holder dies to SIGKILL.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn sigkill_releases_lock() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");

    // Holder: acquire the lock and hold it "forever" by running a long sleep.
    // flock acquires before spawning the child, so once the lock reads as held
    // we know flock owns the fd.
    let mut holder = Holder::spawn(lockfile.path(), &[]);

    // SIGKILL the flock process. Its fd closes on death and the kernel must
    // release the lock — no cleanup code runs on SIGKILL.
    holder.kill();

    // A waiter should now acquire promptly. Give a short timeout so a failure
    // to release surfaces as a conflict-exit rather than hanging forever.
    let start = Instant::now();
    let status = flock_cmd()
        .args(["-w", "5"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .expect("run waiter");
    check!(
        status.success(),
        "lock was not released on SIGKILL: waiter exited {status:?}"
    );
    check!(
        start.elapsed() < Duration::from_secs(5),
        "waiter took too long — lock likely leaked past death"
    );
}

// ---------------------------------------------------------------------------
// Flag behaviour: -n, -w, exit-code propagation, shared vs exclusive.
// ---------------------------------------------------------------------------

#[test]
fn nonblock_fails_fast_when_held() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    let _holder = Holder::spawn(lockfile.path(), &[]);

    let start = Instant::now();
    let status = flock_cmd()
        .arg("-n")
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();

    check!(status.code() == Some(1), "expected default conflict code 1");
    check!(
        start.elapsed() < Duration::from_secs(2),
        "-n did not fail fast"
    );
}

#[test]
fn nonblock_custom_conflict_exit_code() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    let _holder = Holder::spawn(lockfile.path(), &[]);

    let status = flock_cmd()
        .args(["-n", "-E", "7"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(status.code() == Some(7));
}

#[test]
fn timeout_expires_when_held() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    let _holder = Holder::spawn(lockfile.path(), &[]);

    let start = Instant::now();
    let status = flock_cmd()
        .args(["-w", "1"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    let elapsed = start.elapsed();

    check!(
        status.code() == Some(1),
        "expected conflict code on timeout"
    );
    check!(
        elapsed >= Duration::from_secs(1),
        "returned before the timeout elapsed"
    );
    check!(
        elapsed < Duration::from_secs(3),
        "waited well past the timeout"
    );
}

#[test]
fn timeout_acquires_when_freed_in_time() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");

    // Hold the lock briefly in a background process, then a `-w` waiter that
    // outlasts the hold should succeed.
    let mut holder = flock_cmd()
        .arg(lockfile.path())
        .args(["sleep", "1"])
        .spawn()
        .unwrap();
    wait_until_held(lockfile.path(), Duration::from_secs(5));

    let status = flock_cmd()
        .args(["-w", "10"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        status.success(),
        "waiter should acquire once holder releases"
    );

    holder.wait().unwrap();
}

#[test]
fn propagates_child_exit_code() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");

    let status = flock_cmd()
        .arg(lockfile.path())
        .args(["sh", "-c", "exit 42"])
        .status()
        .unwrap();
    check!(status.code() == Some(42));
}

#[test]
fn shared_locks_coexist_but_block_exclusive() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");

    // Hold a shared lock.
    let _holder = Holder::spawn(lockfile.path(), &["-s"]);

    // Another shared lock should acquire immediately (they coexist).
    let shared = flock_cmd()
        .args(["-s", "-n"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(shared.success(), "two shared locks should coexist");

    // An exclusive lock should be refused while a shared lock is held.
    let exclusive = flock_cmd()
        .args(["-x", "-n"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        exclusive.code() == Some(1),
        "exclusive must be blocked by a held shared lock"
    );
}

#[test]
fn creates_lockfile_when_absent_and_keeps_it() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("newlock");
    check!(!lockfile.path().exists());

    let status = flock_cmd()
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(status.success());
    // The file persists after release — we never unlink it.
    check!(
        lockfile.path().exists(),
        "lockfile must persist after release"
    );
}

#[test]
fn does_not_truncate_existing_lockfile() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    fs::write(lockfile.path(), "important preexisting bytes").unwrap();

    let status = flock_cmd()
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(status.success());
    let after = fs::read_to_string(lockfile.path()).unwrap();
    check!(
        after == "important preexisting bytes",
        "lockfile contents must be preserved (never truncated)"
    );
}

#[test]
fn holder_sidecar_reports_label() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");

    let _holder = Holder::spawn(lockfile.path(), &["--label", "test-suite"]);

    // A verbose waiter that can't get the lock should name the holder.
    let output = flock_cmd()
        .args(["-n", "-v"])
        .arg(lockfile.path())
        .args(["true"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    check!(
        stderr.contains("test-suite"),
        "verbose waiter should report the holder label; stderr = {stderr:?}"
    );
}

// ---------------------------------------------------------------------------
// Descriptor mode: `flock <fd>` locks an inherited fd and returns, leaving the
// lock held via the caller's still-open descriptor.
// ---------------------------------------------------------------------------

/// The lock taken on an inherited fd must outlive the tool's exit: it is held by
/// the shell's fd, so another process opening the same file (its own OFD) is
/// blocked even though the flock process has already returned.
#[cfg(unix)]
#[test]
fn fd_mode_lock_survives_tool_exit() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    let _holder = FdHolder::spawn(lockfile.path(), &[]);

    let probe = flock_cmd()
        .arg("-n")
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        probe.code() == Some(1),
        "descriptor-mode lock must persist after the tool exits; probe = {probe:?}"
    );
}

/// SIGKILLing the shell that holds fd 9 closes the fd, so the kernel releases
/// the descriptor-mode lock — the same on-death guarantee as wrap mode, now for
/// the fd form.
#[cfg(unix)]
#[test]
fn fd_mode_sigkill_of_holder_releases() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    let mut holder = FdHolder::spawn(lockfile.path(), &[]);

    check!(
        !is_free(lockfile.path()),
        "lock should be held before the kill"
    );

    holder.kill();

    let start = Instant::now();
    let status = flock_cmd()
        .args(["-w", "5"])
        .arg(lockfile.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        status.success(),
        "descriptor-mode lock was not released on SIGKILL: waiter exited {status:?}"
    );
    check!(
        start.elapsed() < Duration::from_secs(5),
        "waiter took too long — fd lock likely leaked past death"
    );
}

/// `flock -u <fd>` releases the lock explicitly while the holding shell is still
/// alive (fd still open), proving the unlock — not process death — freed it.
#[cfg(unix)]
#[test]
fn fd_mode_unlock_releases_while_holder_alive() {
    let dir = TempDir::new().unwrap();
    let lockfile = dir.child("lock");
    let trigger = dir.child("trigger");

    let bin = env!("CARGO_BIN_EXE_flock");
    // Hold the lock until the trigger file appears, then unlock fd 9 and keep
    // the fd open (still alive) so freeing can only be attributed to `-u`.
    let tail = format!(
        "while [ ! -f {trigger:?} ]; do sleep 0.05; done; {bin:?} -u 9; sleep 30",
        trigger = trigger.path(),
    );
    let mut holder = FdHolder::spawn_script(lockfile.path(), &[], &tail);

    check!(!is_free(lockfile.path()), "should be held before -u");

    fs::write(trigger.path(), "go").unwrap();

    wait_until(lockfile.path(), Duration::from_secs(5), is_free);
    check!(
        holder.child.try_wait().unwrap().is_none(),
        "holder shell should still be alive after -u (unlock, not exit, freed the lock)"
    );

    holder.kill();
}

// ---------------------------------------------------------------------------
// Test helpers.
// ---------------------------------------------------------------------------

/// True if an exclusive lock on `path` can be taken right now (i.e. it's free).
/// Uses a child `flock -n` so we probe the same kernel lock the tool uses,
/// without disturbing it (the probe releases immediately on exit).
fn is_free(path: &Path) -> bool {
    ProcCommand::new(env!("CARGO_BIN_EXE_flock"))
        .arg("-n")
        .arg(path)
        .args(["true"])
        .status()
        .unwrap()
        .success()
}

/// Block until an exclusive lock on `path` is held by someone else, or panic
/// after `timeout`. Works for shared holders too: a shared lock still blocks
/// the exclusive probe.
fn wait_until_held(path: &Path, timeout: Duration) {
    wait_until(path, timeout, |p| !is_free(p));
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

/// A background `flock` process holding a lock (via a long `sleep`), spawned in
/// its own process group so the whole group — flock and its `sleep` child — can
/// be killed together without orphaning the grandchild. Killed on drop.
struct Holder {
    child: std::process::Child,
}

impl Holder {
    /// Spawn a holder with the given extra flock args (e.g. `["-s"]` or
    /// `["--label", "x"]`) and block until the lock reads as held.
    fn spawn(lockfile: &Path, extra_args: &[&str]) -> Self {
        let mut cmd = ProcCommand::new(env!("CARGO_BIN_EXE_flock"));
        cmd.args(extra_args).arg(lockfile).args(["sleep", "30"]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // New process group; the child (flock) becomes the group leader so
            // pgid == its pid, letting `kill -<pid>` reap the whole tree.
            cmd.process_group(0);
        }
        let child = cmd.spawn().expect("spawn holder");
        let holder = Holder { child };
        wait_until_held(lockfile, Duration::from_secs(5));
        holder
    }

    /// SIGKILL the holder's whole process group. flock dies to SIGKILL — the
    /// death-release path — and its `sleep` child dies with it (no orphan).
    fn kill(&mut self) {
        kill_group(&mut self.child);
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        kill_group(&mut self.child);
    }
}

fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Negative target = process group. The holder was spawned as its own
        // group leader, so its pid is the pgid.
        let pid = child.id();
        let _ = ProcCommand::new("kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .status();
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

/// A shell that opens fd 9 on the lockfile, locks it via the `flock` tool in
/// descriptor mode (the tool exits, leaving the lock held by this shell's fd),
/// then keeps the fd open. Spawned in its own process group; killed on drop.
/// Unix only — it relies on shell fd redirection and fd inheritance.
#[cfg(unix)]
struct FdHolder {
    child: std::process::Child,
}

#[cfg(unix)]
impl FdHolder {
    /// Lock fd 9, then hold it open by sleeping.
    fn spawn(lockfile: &Path, flock_args: &[&str]) -> Self {
        Self::spawn_script(lockfile, flock_args, "sleep 30")
    }

    /// Lock fd 9, then run `tail` (with fd 9 still open) — e.g. to unlock later.
    fn spawn_script(lockfile: &Path, flock_args: &[&str], tail: &str) -> Self {
        use std::os::unix::process::CommandExt as _;
        let bin = env!("CARGO_BIN_EXE_flock");
        // exec 9>>LOCK opens fd 9; `flock <args> 9` locks that OFD and exits,
        // leaving the lock held via fd 9 for as long as this shell lives.
        let script = format!(
            "exec 9>>{lockfile:?}; {bin:?} {args} 9 || exit 3; {tail}",
            args = flock_args.join(" "),
        );
        let mut cmd = ProcCommand::new("sh");
        cmd.args(["-c", &script]).process_group(0);
        let child = cmd.spawn().expect("spawn fd holder");
        let holder = FdHolder { child };
        wait_until_held(lockfile, Duration::from_secs(5));
        holder
    }

    fn kill(&mut self) {
        kill_group(&mut self.child);
    }
}

#[cfg(unix)]
impl Drop for FdHolder {
    fn drop(&mut self) {
        kill_group(&mut self.child);
    }
}
