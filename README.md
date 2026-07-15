# flock

`flock` runs a command while holding an atomic, OS-managed, death-safe file
lock. It is a small, portable subset of util-linux / discoteq
[`flock(1)`](https://man7.org/linux/man-pages/man1/flock.1.html), suitable as a
drop-in machine-wide mutex for scripts and test harnesses.

The lock is a real kernel advisory lock (`flock(2)` on unix, `LockFileEx` on
Windows). Two properties are the whole point:

- **Atomic, no TOCTOU.** Acquisition is arbitrated by the kernel on the open
  file description. There is no observe-then-act window, so two processes can
  never both believe they hold the lock.
- **Death-safe.** The lock lives on the open file descriptor, not on the file's
  existence. The kernel releases it the instant the holding fd closes —
  including when the process is `SIGKILL`ed. There is no stale lock file to
  garbage-collect, and `flock` never unlinks the lockfile.

The CLI API follows https://github.com/discoteq/flock , this is just a little
easier to install.

> [!NOTE]
> Entirely vibe coded, use it if you'd like :shrug::robot:

## Usage

`flock` has two forms, mirroring util-linux `flock(1)`.

### Wrap mode — lock around a command

```
flock [OPTIONS] <lockfile> <command> [args...]
```

`flock` opens (creating if absent) `<lockfile>`, acquires an exclusive advisory
lock on it, runs `<command>` as a child while the lock is held, then releases
the lock when the child exits and propagates the child's exit status.

```bash
# Serialize a critical section across processes/machines:
flock /tmp/build.lock ./run-tests.sh

# Fail immediately instead of waiting if the lock is held:
flock -n /tmp/build.lock ./run-tests.sh

# Wait up to 30 seconds, then give up:
flock -w 30 /tmp/build.lock ./run-tests.sh
```

### Descriptor mode — lock an already-open fd

```
flock [OPTIONS] <fd>          # lock the inherited descriptor, then exit
flock -u <fd>                 # release it
```

When the target is a bare integer and no command follows, `flock` takes the
lock on that **already-open file descriptor** and exits immediately, leaving the
lock held. This is the `exec 9>>lockfile; flock 9` shell idiom: the lock
persists past `flock`'s exit because your shell's fd `9` still refers to the
same open file description, and `flock(2)` only releases once every fd to that
description is closed — i.e. when the shell exits or is `SIGKILL`ed. It's the
drop-in replacement for hand-rolled `fcntl.flock`-on-fd helpers.

```bash
exec 9>>/tmp/build.lock   # open fd 9 on the lockfile
flock -n 9 || exit 1      # take the lock (or bail if held); lock stays held
# ... critical section; the lock is released automatically when this shell
#     exits, even on SIGKILL ...
flock -u 9                # optional: release explicitly while still running
```

Descriptor mode is unix-only (it relies on fd inheritance and `flock(2)`).

### Options

| Flag | Description |
|------|-------------|
| `-n`, `--nonblock` | Fail immediately (exit with the conflict code) if the lock is held, instead of waiting. |
| `-w`, `--timeout <SECONDS>` | Wait at most `SECONDS` (fractional allowed) for the lock, then fail with the conflict code. |
| `-s`, `--shared` | Take a shared lock instead of an exclusive one. |
| `-x`, `--exclusive` | Take an exclusive lock (the default; accepted for `flock(1)` parity). |
| `-E`, `--conflict-exit-code <N>` | Exit code to use when `-n`/`-w` cannot acquire the lock (default `1`, matching util-linux). |
| `-u`, `--unlock` | Descriptor mode only: release the lock held on `<fd>` (`flock -u <fd>`). |
| `--label <TEXT>` | Record a human-readable label in the holder sidecar so a waiter can report who holds the lock (wrap mode). |
| `-v`, `--verbose` | Log lock waiting/acquisition to stderr. |

### Exit codes

- The **child's exit code** on success (a signalled child yields `128 + signum`
  on unix).
- The **conflict exit code** (default `1`, override with `-E`) when `-n`/`-w`
  cannot acquire the lock.
- `127` if the command cannot be found, `126` if it cannot be executed, `125`
  if `flock` itself fails.

### Holder diagnostics

In wrap mode, while the lock is held `flock` writes a best-effort
`<lockfile>.holder` sidecar containing the holder's pid, `--label`, start time,
and lock mode (descriptor mode has no path to write it and skips it). A
waiter run with `-v` reports "held by …" from it. The sidecar is purely
diagnostic — it is never consulted for mutual exclusion, so a stale sidecar
left by a `SIGKILL`ed holder is harmless and is overwritten by the next holder.

### Installation

#### Install prebuilt binaries via shell script

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/quodlibetor/flock/releases/latest/download/flock-installer.sh | sh
```

#### Install prebuilt binaries via powershell script

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/quodlibetor/flock/releases/latest/download/flock-installer.ps1 | iex"
```

#### Install prebuilt binaries via Homebrew

```bash
brew install quodlibetor/tap/flock
```

#### Install via mise

`flock`'s releases (built by `dist`) ship platform archives on GitHub Releases,
so mise can install them with its `github:` (ubi) backend:

```bash
mise use -g github:quodlibetor/flock
```

## Copying

All code is available under the MIT or Apache 2.0 license, at your option.

## Development

### Running tests

```bash
mise run test        # full suite (unit + integration)
mise run test-unit   # unit tests only
mise run test-int    # integration tests only
```

The integration suite spawns real `flock` child processes to prove the
mutual-exclusion and death-release properties end-to-end.

### Performing a release

Ensure git-cliff and cargo-release are both installed (run `mise install` to
get them) and run `cargo release [patch|minor]`.

If things look good, run again with `--execute`. Pushing the resulting `vX.Y.Z`
tag triggers the `dist` release workflow, which builds the per-platform
binaries and installers and publishes them to a GitHub Release.
