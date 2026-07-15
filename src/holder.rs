//! Best-effort holder diagnostics.
//!
//! While the lock is held we write a `<lockfile>.holder` sidecar describing who
//! holds it (pid, optional label, start time, mode) so a waiter can report
//! "held by …". This is *purely diagnostic*: it is never consulted for mutual
//! exclusion, so a stale sidecar (left by a `SIGKILL`ed holder) is harmless —
//! the next holder overwrites it. Any I/O error here is swallowed; failing to
//! write the sidecar must never affect locking.

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

/// Removes the holder sidecar when dropped (on a clean, non-`SIGKILL` exit).
pub struct Guard {
    path: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn sidecar_path(lockfile: &Path) -> PathBuf {
    let mut name = lockfile.file_name().unwrap_or_default().to_os_string();
    name.push(".holder");
    lockfile.with_file_name(name)
}

/// Write the holder sidecar. Returns a guard that removes it on drop, or
/// `None` if the sidecar could not be written (which is fine — it's advisory).
pub fn write(lockfile: &Path, label: Option<&str>, shared: bool) -> Option<Guard> {
    let path = sidecar_path(lockfile);
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let contents = format!(
        "pid={}\nlabel={}\nstarted={}\nmode={}\n",
        process::id(),
        label.unwrap_or(""),
        started,
        if shared { "shared" } else { "exclusive" },
    );

    // Write to a pid-unique temp file then rename, so a reader never sees a
    // half-written sidecar. Rename within a directory is atomic.
    let tmp = lockfile.with_file_name(format!(
        "{}.holder.tmp.{}",
        lockfile.file_name().unwrap_or_default().to_string_lossy(),
        process::id(),
    ));
    if fs::write(&tmp, contents.as_bytes()).is_err() {
        let _ = fs::remove_file(&tmp);
        return None;
    }
    if fs::rename(&tmp, &path).is_err() {
        let _ = fs::remove_file(&tmp);
        return None;
    }
    Some(Guard { path })
}

/// Read the holder sidecar into a compact one-line summary for `-v` output.
/// Returns `None` if there is no readable sidecar.
pub fn read(lockfile: &Path) -> Option<String> {
    let contents = fs::read_to_string(sidecar_path(lockfile)).ok()?;
    let mut pid = "";
    let mut label = "";
    for line in contents.lines() {
        if let Some(v) = line.strip_prefix("pid=") {
            pid = v;
        } else if let Some(v) = line.strip_prefix("label=") {
            label = v;
        }
    }
    if pid.is_empty() {
        return None;
    }
    if label.is_empty() {
        Some(format!("pid {pid}"))
    } else {
        Some(format!("pid {pid}, {label}"))
    }
}
