//! `flock` — run a command while holding an atomic, OS-managed, death-safe
//! file lock.
//!
//! This is a portable subset of util-linux/discoteq `flock(1)`. The lock is a
//! kernel advisory lock taken through std's `File::lock` family (`flock(2)` on
//! unix, `LockFileEx` on Windows). Two properties matter:
//!
//! - **Atomic / no TOCTOU.** Acquisition is arbitrated by the kernel on the
//!   open file description. There is no observe-then-act window a second waiter
//!   could exploit, so two holders can never both believe they own the lock.
//! - **Death-safe.** The lock lives on the open fd, not on the file's
//!   existence. The kernel releases it the instant the holding fd is closed —
//!   including when the process dies to `SIGKILL`. There is no stale-lock file
//!   to garbage-collect, and we never unlink the lockfile.
//!
//! Two forms, mirroring util-linux `flock(1)`:
//!
//! - **Wrap mode** — `flock [opts] <lockfile> <command> [args…]`: open/create
//!   the lockfile, hold the lock for the child's lifetime, then release.
//! - **Descriptor mode** — `flock [opts] <fd>` (a bare integer, no command):
//!   take the lock on an already-open, inherited descriptor and exit
//!   immediately, leaving the lock held. It survives our exit because the
//!   caller's fd (the shell's `exec 9>>lockfile`) still refers to the same open
//!   file description; `flock(2)` releases only once every fd to that OFD is
//!   closed (shell exit / `SIGKILL`). `-u` releases explicitly.

use std::fs::{OpenOptions, TryLockError};
use std::io::{self, ErrorKind};
use std::path::Path;
use std::process::Command as ProcCommand;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;

mod holder;

/// Run a command while holding an OS-managed file lock, or lock an inherited
/// file descriptor.
///
/// Wrap mode — `flock [opts] <lockfile> <command> [args...]`: acquire an
/// advisory lock on <lockfile> (creating it if absent, never truncating or
/// deleting it), run <command> as a child while the lock is held, then release
/// the lock when the child exits and propagate its exit status.
///
/// Descriptor mode — `flock [opts] <fd>` with no command: take the lock on the
/// already-open descriptor <fd> and exit immediately, leaving the lock held via
/// the caller's still-open fd (the `exec 9>>lockfile; flock 9` shell pattern).
/// `flock -u <fd>` releases it.
#[derive(Debug, Parser)]
#[command(name = "flock", version, verbatim_doc_comment)]
struct Opts {
    /// Fail immediately (exit with the conflict code) if the lock is held,
    /// rather than waiting.
    #[arg(short = 'n', long, conflicts_with = "timeout")]
    nonblock: bool,

    /// Wait at most this many seconds for the lock, then fail with the
    /// conflict code. Fractional seconds are allowed.
    #[arg(short = 'w', long, value_name = "SECONDS")]
    timeout: Option<f64>,

    /// Take a shared lock instead of an exclusive one.
    #[arg(short = 's', long, conflicts_with = "exclusive")]
    shared: bool,

    /// Take an exclusive lock (the default; accepted for `flock(1)` parity).
    #[arg(short = 'x', long)]
    exclusive: bool,

    /// Exit code to use when `-n`/`-w` cannot acquire the lock.
    #[arg(
        short = 'E',
        long = "conflict-exit-code",
        value_name = "N",
        default_value_t = 1
    )]
    conflict_exit_code: i32,

    /// Human-readable label recorded in the holder sidecar so a waiter can
    /// report who holds the lock.
    #[arg(long, value_name = "TEXT")]
    label: Option<String>,

    /// Drop a lock held on an inherited descriptor (descriptor mode only):
    /// `flock -u <fd>`.
    #[arg(short = 'u', long)]
    unlock: bool,

    /// Log lock waiting/acquisition to stderr.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Lock target: a file path (wrap mode, created if absent and never
    /// truncated or unlinked) when a command follows, or a bare integer file
    /// descriptor (descriptor mode) when no command follows.
    target: String,

    /// Command to run while the lock is held, plus its arguments. Omit it to
    /// use descriptor mode on <target>.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 0..,
        value_name = "COMMAND"
    )]
    command: Vec<String>,
}

fn main() {
    let opts = Opts::parse();
    let code = match run(opts) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("flock: {err:#}");
            EXIT_INTERNAL_ERROR
        }
    };
    std::process::exit(code);
}

/// Exit code when flock itself (not the child) fails in an unexpected way.
const EXIT_INTERNAL_ERROR: i32 = 125;
/// Exit code when the command is found but cannot be executed (mirrors sh).
const EXIT_CANNOT_EXECUTE: i32 = 126;
/// Exit code when the command cannot be found (mirrors sh).
const EXIT_COMMAND_NOT_FOUND: i32 = 127;

fn run(opts: Opts) -> Result<i32> {
    // Disambiguation matches util-linux: a bare integer with no command is a
    // descriptor to lock; anything with a trailing command is wrap mode (so a
    // file literally named "9" is lockable as `flock 9 <command>`).
    if opts.command.is_empty() {
        run_fd_mode(&opts)
    } else {
        if opts.unlock {
            bail!("-u/--unlock is only valid in descriptor mode (no command)");
        }
        run_wrap_mode(&opts)
    }
}

/// Wrap mode: hold the lock on `<lockfile>` for the child command's lifetime.
fn run_wrap_mode(opts: &Opts) -> Result<i32> {
    let lockfile = Path::new(&opts.target);
    let file = open_lockfile(lockfile)
        .with_context(|| format!("opening lock file {}", lockfile.display()))?;

    let started = Instant::now();
    let desc = lockfile.display().to_string();
    let acquired = acquire(&file, opts, &desc, Some(lockfile)).context("acquiring lock")?;
    if !acquired {
        // `flock(1)` is silent on a failed `-n`/`-w`; only speak when asked.
        if opts.verbose {
            match holder::read(lockfile) {
                Some(info) => eprintln!("flock: failed to acquire lock (held by {info})"),
                None => eprintln!("flock: failed to acquire lock"),
            }
        }
        return Ok(opts.conflict_exit_code);
    }
    if opts.verbose {
        eprintln!(
            "flock: acquired lock after {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }

    // The lock is held as long as `file` is alive. The sidecar is purely
    // diagnostic and is dropped (removed) at the end of this scope, before the
    // lock's fd is closed. Neither is on the mutual-exclusion path.
    let _holder = holder::write(lockfile, opts.label.as_deref(), opts.shared);

    let status = ProcCommand::new(&opts.command[0])
        .args(&opts.command[1..])
        .status();

    let code = match status {
        Ok(status) => exit_code_of(status),
        Err(err) => {
            eprintln!("flock: {}: {}", opts.command[0], err);
            match err.kind() {
                ErrorKind::NotFound => EXIT_COMMAND_NOT_FOUND,
                ErrorKind::PermissionDenied => EXIT_CANNOT_EXECUTE,
                _ => EXIT_CANNOT_EXECUTE,
            }
        }
    };

    // `_holder` and `file` drop here — sidecar removed, then lock released.
    Ok(code)
}

/// Descriptor mode: lock (or, with `-u`, unlock) an inherited fd, then return.
///
/// The lock is taken on the borrowed descriptor and left held; it persists past
/// our exit because the caller still holds an fd to the same open file
/// description. We wrap the fd in `ManuallyDrop<File>` so `File::drop` never
/// `close(2)`s the caller's descriptor. (Even a stray close wouldn't release the
/// lock while the caller's fd is open — `flock(2)` is per-OFD — but borrowing
/// without owning is the correct model and avoids surprising the caller.)
#[cfg(unix)]
fn run_fd_mode(opts: &Opts) -> Result<i32> {
    use std::mem::ManuallyDrop;
    use std::os::unix::io::FromRawFd as _;

    let fd: i32 = opts.target.parse().map_err(|_| {
        anyhow::anyhow!(
            "expected a command after the lock target, or a bare integer file \
             descriptor for descriptor mode; got {:?}",
            opts.target
        )
    })?;

    // SAFETY: we borrow an fd the caller passed us. ManuallyDrop keeps
    // `File::drop` from closing it; we never read/write it, only lock it.
    let file = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });

    if opts.unlock {
        file.unlock()
            .with_context(|| format!("unlocking fd {fd}"))?;
        if opts.verbose {
            eprintln!("flock: released lock on fd {fd}");
        }
        return Ok(0);
    }

    let desc = format!("fd {fd}");
    let acquired =
        acquire(&file, opts, &desc, None).with_context(|| format!("acquiring lock on fd {fd}"))?;
    if !acquired {
        if opts.verbose {
            eprintln!("flock: failed to acquire lock on fd {fd}");
        }
        return Ok(opts.conflict_exit_code);
    }
    if opts.verbose {
        eprintln!("flock: acquired lock on fd {fd}");
    }
    // Return with the lock held. `file` is ManuallyDrop, so leaving this scope
    // does not close the caller's descriptor; the lock lives on until the
    // caller's fd closes (shell exit / SIGKILL) or `flock -u <fd>` runs.
    Ok(0)
}

#[cfg(not(unix))]
fn run_fd_mode(_opts: &Opts) -> Result<i32> {
    bail!("descriptor lock mode is only supported on unix");
}

/// Open (or create) the lock file. Never truncates it: the file is just an
/// inode to hang the kernel lock on, and truncating it would race with other
/// holders. Falls back to read-only if the path can't be opened writable
/// (e.g. a read-only filesystem or a directory used purely as a lock target).
fn open_lockfile(path: &Path) -> io::Result<std::fs::File> {
    // `truncate(false)` is load-bearing: the lockfile is just an inode to hang
    // the kernel lock on. Truncating it on open would both discard any holder
    // content and race with concurrent openers.
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(_) => OpenOptions::new().read(true).open(path),
    }
}

/// Try to take the lock according to the blocking policy in `opts`.
///
/// `desc` describes the lock target for `-v` messages; `holder_path`, when set
/// (wrap mode), lets a verbose wait report the sidecar's holder.
///
/// Returns `Ok(true)` when the lock is held, `Ok(false)` when it could not be
/// acquired under `-n`/`-w`, and `Err` only on an unexpected lock error.
fn acquire(
    file: &std::fs::File,
    opts: &Opts,
    desc: &str,
    holder_path: Option<&Path>,
) -> Result<bool> {
    // A cheap non-blocking attempt first: it satisfies `-n`, gives `-w` a
    // fast path, and lets the common uncontended case skip the helper thread.
    match try_lock(file, opts.shared)? {
        true => return Ok(true),
        false if opts.nonblock => return Ok(false),
        false => {}
    }

    let Some(timeout) = opts.timeout else {
        // Block indefinitely on this thread.
        if opts.verbose {
            eprint_waiting(desc, holder_path);
        }
        blocking_lock(file, opts.shared)?;
        return Ok(true);
    };

    if timeout <= 0.0 {
        return Ok(false);
    }
    if opts.verbose {
        eprint_waiting(desc, holder_path);
    }
    lock_with_timeout(file, opts.shared, Duration::from_secs_f64(timeout))
}

/// One non-blocking lock attempt. `Ok(false)` means "held by someone else".
///
/// std's `try_lock`/`try_lock_shared` distinguish contention
/// (`TryLockError::WouldBlock`) from a genuine I/O failure
/// (`TryLockError::Error`); only the former is a lock conflict.
fn try_lock(file: &std::fs::File, shared: bool) -> Result<bool> {
    let res = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match res {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err.into()),
    }
}

fn blocking_lock(file: &std::fs::File, shared: bool) -> Result<()> {
    if shared {
        file.lock_shared()?
    } else {
        file.lock()?
    }
    Ok(())
}

/// Block up to `timeout` for the lock by doing a blocking acquire on a helper
/// thread and waiting on it with a deadline.
///
/// A blocking lock cannot be cancelled from another thread, so on timeout we
/// abandon the helper: `main` returns the conflict code and the process exits,
/// which closes the fd and releases anything the helper may have raced to grab.
/// Because the lock lives on the fd, no lock can leak past our exit.
fn lock_with_timeout(file: &std::fs::File, shared: bool, timeout: Duration) -> Result<bool> {
    // The helper needs the same open file description. `try_clone` dups the fd;
    // std documents that the lock is shared across dups and released only once
    // this file and every dup are closed — both fds are dropped before this
    // process exits, so the lock still frees.
    let helper_file = file
        .try_clone()
        .context("cloning lock fd for timeout wait")?;
    let (tx, rx) = mpsc::channel();
    let builder = thread::Builder::new().name("flock-wait".into());
    builder
        .spawn(move || {
            let res = if shared {
                helper_file.lock_shared()
            } else {
                helper_file.lock()
            };
            // If the receiver already timed out, this send just fails; the
            // helper_file drops here and its dup fd closes.
            let _ = tx.send(res);
        })
        .context("spawning lock wait thread")?;

    match rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(true),
        Ok(Err(err)) => Err(err.into()),
        Err(RecvTimeoutError::Timeout) => Ok(false),
        Err(RecvTimeoutError::Disconnected) => Ok(false),
    }
}

fn eprint_waiting(desc: &str, holder_path: Option<&Path>) {
    match holder_path.and_then(holder::read) {
        Some(info) => eprintln!("flock: waiting for lock on {desc} (held by {info})"),
        None => eprintln!("flock: waiting for lock on {desc}"),
    }
}

/// Map a child's exit status to the code flock should exit with. On unix a
/// signalled child yields 128 + signum, matching a shell.
fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    EXIT_CANNOT_EXECUTE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_uses_child_code() {
        // We can't easily fabricate an ExitStatus, so exercise the code path
        // via a real short-lived process.
        let status = ProcCommand::new("sh")
            .args(["-c", "exit 42"])
            .status()
            .unwrap();
        assert_eq!(exit_code_of(status), 42);
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_maps_signal() {
        let status = ProcCommand::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(exit_code_of(status), 128 + 15);
    }
}
