//! End-to-end tests for *descriptor-mode* weighted acquisition — the fd
//! analogue of `weighted.rs`. The caller (a shell) opens the pool's files as
//! inherited descriptors and `flock --tokens K --pool N --lock-fd <fd>
//! [--slot-fds <spec>] <lockfile>` takes the tokens on them and exits, leaving
//! the locks held by the caller's still-open fds. The proofs mirror wrap mode:
//!
//! - `fd_writer_holds_and_excludes`: a full-pool writer on an inherited fd holds
//!   the main lock exclusive after the tool exits, blocking everyone.
//! - `fd_reader_holds_shared_and_reports_slots`: a reader holds main shared
//!   (coexists with a shared probe, blocks an exclusive one) and prints the
//!   slot fds it locked.
//! - `fd_reader_interops_with_wrap_reader`: an fd reader and a wrap reader draw
//!   from the *same* pool — together they exhaust it and a further reader is
//!   refused.
//! - `fd_full_pool_excludes_wrap_tokens` / `wrap_tokens_exclude_fd_full_pool`:
//!   the two modes are mutually exclusive through the shared main lock.
//! - `fd_sigkill_of_caller_frees_everything`: SIGKILLing the holding shell frees
//!   the main lock *and* every slot it held.
//! - `fd_unlock_releases_while_holder_alive`: `flock -u <fd>` per held fd frees
//!   the tokens without the holder exiting.
//! - error-path tests for the misuse the CLI rejects.

use std::fs;
use std::path::Path;
use std::process::{Child, Command as ProcCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_fs::TempDir;
use assert_fs::prelude::*;
use assert2::check;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_flock")
}

fn flock() -> ProcCommand {
    ProcCommand::new(bin())
}

/// True if an exclusive lock on the *main* lockfile can be taken right now. A
/// token reader holds it shared and a writer holds it exclusive, so this is
/// false whenever any token holder (wrap or fd) is active.
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

fn kill_group(child: &mut Child) {
    let pid = child.id();
    let _ = ProcCommand::new("kill")
        .arg("-KILL")
        .arg(format!("-{pid}"))
        .stderr(Stdio::null())
        .status();
    let _ = child.wait();
}

/// Build a POSIX-sh script that opens fd 9 on the lockfile and — for a reader —
/// fds `10..10+pool` on the `pool` slot files, runs the fd-weighted acquire
/// (with any `extra` flock flags, e.g. `-n`), and then runs `tail` with every fd
/// still open. Slot fds are contiguous from 10 so `--slot-fds 10-<last>` names
/// them in order.
fn holder_script(lock: &Path, tokens: usize, pool: usize, extra: &str, tail: &str) -> String {
    let bin = bin();
    let lock = lock.display();
    if tokens == pool {
        // Writer: only the main fd, no slots.
        format!(
            r#"exec 9<>"{lock}"
"{bin}" --tokens {tokens} --pool {pool} --lock-fd 9 {extra} "{lock}" || exit 3
{tail}"#
        )
    } else {
        let last = 10 + pool - 1;
        format!(
            r#"exec 9<>"{lock}"
mkdir -p "{lock}.slots"
i=0
while [ $i -lt {pool} ]; do
  fd=$(( 10 + i ))
  slot=$(printf '%02d' "$i")
  eval "exec ${{fd}}<>\"{lock}.slots/${{slot}}\""
  i=$(( i + 1 ))
done
"{bin}" --tokens {tokens} --pool {pool} --lock-fd 9 --slot-fds 10-{last} {extra} "{lock}" || exit 3
{tail}"#
        )
    }
}

/// A background shell holding `tokens/pool` on inherited fds (via a long sleep),
/// spawned in its own process group so the whole tree can be SIGKILLed. Killed
/// on drop.
struct FdHolder {
    child: Child,
}

impl FdHolder {
    fn spawn(lock: &Path, tokens: usize, pool: usize) -> Self {
        Self::spawn_tail(lock, tokens, pool, "sleep 30")
    }

    fn spawn_tail(lock: &Path, tokens: usize, pool: usize, tail: &str) -> Self {
        use std::os::unix::process::CommandExt as _;
        let script = holder_script(lock, tokens, pool, "", tail);
        let mut cmd = ProcCommand::new("sh");
        // Silence the acquire's held-fd report; these background holders exist
        // for their locks, not their stdout.
        cmd.args(["-c", &script])
            .stdout(Stdio::null())
            .process_group(0);
        let child = cmd.spawn().expect("spawn fd holder");
        let holder = FdHolder { child };
        wait_until(lock, Duration::from_secs(5), |p| !main_lock_free(p));
        holder
    }

    fn kill(&mut self) {
        kill_group(&mut self.child);
    }
}

impl Drop for FdHolder {
    fn drop(&mut self) {
        kill_group(&mut self.child);
    }
}

// ---------------------------------------------------------------------------

/// A full-pool writer taken on an inherited fd stays held after the tool exits,
/// and — holding the main lock exclusive — blocks any other acquirer.
#[test]
fn fd_writer_holds_and_excludes() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");
    let _writer = FdHolder::spawn(lock.path(), 4, 4);

    check!(
        !main_lock_free(lock.path()),
        "writer should hold main exclusive"
    );

    // Even a shared probe is blocked — a full-pool writer is exclusive.
    let shared = flock()
        .args(["-s", "-n"])
        .arg(lock.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        shared.code() == Some(1),
        "a shared probe must be blocked by a full-pool fd writer"
    );
}

/// A reader holds the main lock *shared* (a shared probe coexists, an exclusive
/// one is blocked) and prints the slot fds it locked.
#[test]
fn fd_reader_holds_shared_and_reports_slots() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");

    // Capture the acquire's stdout (the held slot fds) via a non-sleeping run.
    // Open 4 slot fds, take 2 tokens, print held fds, then immediately release.
    let script = holder_script(lock.path(), 2, 4, "", "true");
    let out = ProcCommand::new("sh")
        .args(["-c", &script])
        .output()
        .unwrap();
    check!(out.status.success(), "reader acquire failed: {out:?}");
    let reported = String::from_utf8_lossy(&out.stdout);
    let fds: Vec<&str> = reported.split_whitespace().collect();
    check!(
        fds.len() == 2,
        "reader should report the 2 slot fds it locked; got {reported:?}"
    );
    for fd in &fds {
        let n: i32 = fd.parse().expect("held fd should be an integer");
        check!(
            (10..=13).contains(&n),
            "held fd {n} outside the opened 10-13"
        );
    }

    // Now a live reader: main is shared while it is held.
    let _reader = FdHolder::spawn(lock.path(), 2, 4);
    let shared = flock()
        .args(["-s", "-n"])
        .arg(lock.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        shared.success(),
        "a shared probe must coexist with an fd reader"
    );
    let excl = flock()
        .args(["-x", "-n"])
        .arg(lock.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        excl.code() == Some(1),
        "an exclusive probe must be blocked by an fd reader holding main shared"
    );
}

/// Interop: an fd reader and a wrap reader draw from the *same* pool. An fd
/// reader holding 2/4 leaves 2 slots; a wrap reader taking 2/4 fits, but a
/// further 1-token reader would overflow and is refused.
#[test]
fn fd_reader_interops_with_wrap_reader() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");

    let _fd_reader = FdHolder::spawn(lock.path(), 2, 4);

    // A wrap reader for the last 2 slots fits.
    let mut wrap_reader = {
        use std::os::unix::process::CommandExt as _;
        flock()
            .args(["--tokens", "2", "--pool", "4"])
            .arg(lock.path())
            .args(["sleep", "30"])
            .process_group(0)
            .spawn()
            .unwrap()
    };
    // Give it a moment to grab its slots.
    thread::sleep(Duration::from_millis(300));

    // The pool is now full (2 fd + 2 wrap). A further reader is refused fast.
    let overflow = flock()
        .args(["--tokens", "1", "--pool", "4", "-n"])
        .arg(lock.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        overflow.code() == Some(1),
        "pool should be full (2 fd + 2 wrap tokens); a further reader must be refused"
    );

    kill_group(&mut wrap_reader);
}

/// A full-pool writer on an inherited fd excludes a wrap-mode token reader.
#[test]
fn fd_full_pool_excludes_wrap_tokens() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");
    let _writer = FdHolder::spawn(lock.path(), 4, 4);

    let reader = flock()
        .args(["--tokens", "1", "--pool", "4", "-n"])
        .arg(lock.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        reader.code() == Some(1),
        "a wrap token reader must be blocked by an fd full-pool writer"
    );
}

/// The other direction: a wrap-mode token reader excludes an fd full-pool
/// writer (which must wait for the shared main lock to drain).
#[test]
fn wrap_tokens_exclude_fd_full_pool() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");

    // A wrap reader holds main shared.
    let mut wrap_reader = {
        use std::os::unix::process::CommandExt as _;
        flock()
            .args(["--tokens", "1", "--pool", "4"])
            .arg(lock.path())
            .args(["sleep", "30"])
            .process_group(0)
            .spawn()
            .unwrap()
    };
    wait_until(lock.path(), Duration::from_secs(5), |p| !main_lock_free(p));

    // An fd full-pool writer, non-blocking, cannot drain the pool → refused.
    let script = holder_script(lock.path(), 4, 4, "-n", "true");
    let writer = ProcCommand::new("sh")
        .args(["-c", &script])
        .status()
        .unwrap();
    check!(
        !writer.success(),
        "an fd full-pool writer must be refused while a wrap reader holds a token"
    );

    kill_group(&mut wrap_reader);
}

/// SIGKILLing the holding shell frees the main lock AND every slot it held: two
/// fresh readers that together need the whole pool then fit — only possible if
/// the killed holder's slots were released.
#[test]
fn fd_sigkill_of_caller_frees_everything() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");

    let mut victim = FdHolder::spawn(lock.path(), 2, 4);
    check!(!main_lock_free(lock.path()), "held before the kill");
    victim.kill();

    // Main frees promptly.
    wait_until(lock.path(), Duration::from_secs(5), main_lock_free);

    // Prove the slots freed too: a 4/4 writer (needs the whole pool exclusive)
    // acquires promptly.
    let start = Instant::now();
    let writer = ProcCommand::new("sh")
        .args(["-c", &holder_script(lock.path(), 4, 4, "", "true")])
        .status()
        .unwrap();
    check!(
        writer.success(),
        "killed fd holder's tokens leaked — a full-pool writer could not acquire"
    );
    check!(
        start.elapsed() < Duration::from_secs(3),
        "slot reuse was not prompt"
    );
}

/// A refused `-n` reader must leave NO residual lock behind — descriptor mode is
/// all-or-nothing. We force a *partial* acquisition (main-shared + one swept
/// slot) that then can't complete, keep the refused caller's fds open, and prove
/// the pool is fully reusable: a residual main-shared would block an exclusive
/// (full-pool-writer-equivalent) acquire, and a residual slot would keep the
/// pool one token short.
///
/// This drives the shared `BorrowedLockGuard` in `acquire_fd` down its armed
/// `Drop` path (the reader returns `None` while holding main + a swept slot on
/// the caller's fds). The genuine-I/O-`Err` exit (`?` after main is acquired)
/// releases through the *same* guard `Drop`; it is not separately tested because
/// inducing a real `flock(2)`/open failure mid-acquire deterministically needs
/// root-sensitive filesystem contrivances (a chmod'd dir is bypassed by root),
/// a more fragile test than the uniform guard it would exercise.
#[test]
fn fd_refused_reader_leaves_no_residual() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");
    let ready = dir.child("ready");
    let bin = bin();

    // Occupy 3 of 4 slots so a 2-token reader grabs the 1 free slot, then stalls
    // and is refused — the exact partial-acquisition path.
    let mut occupant = FdHolder::spawn(lock.path(), 3, 4);

    // A reader that opens all its fds, is refused (-n) for 2 tokens, then keeps
    // its fds open. If the acquire leaked main-shared or the swept slot, those
    // would linger on these still-open fds.
    let refused_script = format!(
        r#"exec 9<>"{lock}"
mkdir -p "{lock}.slots"
for i in 0 1 2 3; do
  slot=$(printf '%02d' "$i")
  eval "exec $((10 + i))<>\"{lock}.slots/${{slot}}\""
done
"{bin}" --tokens 2 --pool 4 -n --lock-fd 9 --slot-fds 10-13 "{lock}" && exit 9
: > "{ready}"
sleep 30"#,
        lock = lock.path().display(),
        ready = ready.path().display(),
    );
    let mut refused = {
        use std::os::unix::process::CommandExt as _;
        ProcCommand::new("sh")
            .args(["-c", &refused_script])
            .stdout(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    };
    // The `ready` file marks that the refusal already returned, fds still open.
    wait_until(ready.path(), Duration::from_secs(5), |p| p.exists());

    // Release the occupant. The pool is now empty UNLESS the refused reader left
    // a lock on its still-open fds.
    occupant.kill();
    wait_until(lock.path(), Duration::from_secs(5), main_lock_free);

    // No leaked main-shared: an exclusive acquire (a full-pool writer's effect on
    // the main lock) succeeds.
    let excl = flock()
        .args(["-x", "-n"])
        .arg(lock.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        excl.success(),
        "refused reader leaked its main-shared lock — an exclusive acquire was blocked"
    );

    // No leaked slot: the whole 4-token pool is allocatable again. A 3-token
    // reader plus a 1-token reader fit only if all four slots are free.
    let mut three = {
        use std::os::unix::process::CommandExt as _;
        flock()
            .args(["--tokens", "3", "--pool", "4"])
            .arg(lock.path())
            .args(["sleep", "30"])
            .process_group(0)
            .spawn()
            .unwrap()
    };
    wait_until(lock.path(), Duration::from_secs(5), |p| !main_lock_free(p));
    let one = flock()
        .args(["--tokens", "1", "--pool", "4", "-w", "3"])
        .arg(lock.path())
        .args(["true"])
        .status()
        .unwrap();
    check!(
        one.success(),
        "refused reader leaked a slot — the pool was one token short"
    );

    kill_group(&mut three);
    kill_group(&mut refused);
}

/// `flock -u <fd>` on each held descriptor frees the tokens while the holding
/// shell is still alive — proving the unlock, not process death, freed them.
#[test]
fn fd_unlock_releases_while_holder_alive() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");
    let trigger = dir.child("trigger");
    let bin = bin();

    // Reader holds 2/4 on fds 9 (main) + 10-13 (slots). When the trigger
    // appears, unlock the main fd and all slot fds, then keep the shell (and its
    // now-unlocked fds) alive so only `-u` can explain the release.
    let tail = format!(
        r#"while [ ! -f "{trigger}" ]; do sleep 0.05; done
for fd in 9 10 11 12 13; do "{bin}" -u "$fd"; done
sleep 30"#,
        trigger = trigger.path().display(),
    );
    let mut holder = FdHolder::spawn_tail(lock.path(), 2, 4, &tail);

    check!(!main_lock_free(lock.path()), "held before -u");
    fs::write(trigger.path(), "go").unwrap();

    wait_until(lock.path(), Duration::from_secs(5), main_lock_free);
    check!(
        holder.child.try_wait().unwrap().is_none(),
        "holder shell should still be alive after -u (unlock, not exit, freed it)"
    );

    holder.kill();
}

// ---------------------------------------------------------------------------
// Misuse the CLI rejects.
// ---------------------------------------------------------------------------

#[test]
fn fd_reader_without_slot_fds_is_rejected() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");
    let out = flock()
        .args(["--tokens", "2", "--pool", "4", "--lock-fd", "9"])
        .arg(lock.path())
        .output()
        .unwrap();
    check!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    check!(err.contains("--slot-fds"), "stderr = {err:?}");
}

#[test]
fn fd_writer_with_slot_fds_is_rejected() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");
    let out = flock()
        .args([
            "--tokens",
            "4",
            "--pool",
            "4",
            "--lock-fd",
            "9",
            "--slot-fds",
            "10-13",
        ])
        .arg(lock.path())
        .output()
        .unwrap();
    check!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    check!(err.contains("takes no slots"), "stderr = {err:?}");
}

#[test]
fn fd_slot_fds_wrong_count_is_rejected() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");
    let out = flock()
        .args([
            "--tokens",
            "2",
            "--pool",
            "4",
            "--lock-fd",
            "9",
            "--slot-fds",
            "10-12",
        ])
        .arg(lock.path())
        .output()
        .unwrap();
    check!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    check!(err.contains("exactly"), "stderr = {err:?}");
}

#[test]
fn fd_weighted_without_lock_fd_is_rejected() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");
    let out = flock()
        .args(["--tokens", "2", "--pool", "4", "--slot-fds", "10-13"])
        .arg(lock.path())
        .output()
        .unwrap();
    // clap enforces `--slot-fds requires --lock-fd`.
    check!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    check!(
        err.contains("lock-fd") || err.contains("lock_fd"),
        "stderr = {err:?}"
    );
}

#[test]
fn fd_unlock_with_tokens_is_rejected() {
    let dir = TempDir::new().unwrap();
    let lock = dir.child("lock");
    let out = flock()
        .args(["-u", "--tokens", "2", "--pool", "4", "--lock-fd", "9"])
        .arg(lock.path())
        .output()
        .unwrap();
    check!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    check!(err.contains("flock -u"), "stderr = {err:?}");
}
