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

### Weighted mode — a token pool instead of a plain lock

```
flock --tokens K --pool N <lockfile> <command> [args...]
```

Instead of one exclusive lock, `--tokens K --pool N` treats the lockfile as a
counting semaphore of `N` tokens and takes `K` of them for the child's
lifetime. This serializes a shared resource by *weight* rather than by "one
command at a time":

- `K < N` is a **shared reader** — several run concurrently as long as their
  tokens sum to at most `N`.
- `K == N` is a **writer** — it drains the whole pool, so it is exclusive.
- A reader that would push the pool past `N` waits (or, with `-n`/`-w`, fails)
  until enough tokens free up.

```bash
# 16-token pool: a unit suite takes ~5, a big run takes all 16.
flock --tokens 5  --pool 16 /tmp/box.lock ./unit-tests.sh    # shared
flock --tokens 16 --pool 16 /tmp/box.lock ./full-e2e.sh      # exclusive
```

**No writer starvation.** A full-pool writer publishes intent on an entry gate
and holds it for the duration; new readers block at the gate, so the pool
drains and the writer acquires within one drain rather than being starved by a
continuous reader stream.

**Death-safety is unchanged.** Every token is an open fd with a kernel lock, so
a `SIGKILL`ed holder frees all of its tokens immediately — no stale slot files
to garbage-collect.

**Interoperates with plain exclusive holders.** Weighted mode keeps the main
lockfile itself in the protocol: readers hold it *shared*, a full-pool writer
holds it *exclusive*. So a process taking a plain `flock <lockfile>` exclusive
lock (e.g. an older binary that predates `--tokens`) blocks every token holder,
and any token holder blocks it. Old and new callers stay mutually correct on
the same lockfile — but all callers that *do* use weighted mode must pass the
**same `--pool`**, since the pool size defines the slot set.

Derived paths: weighted mode creates siblings of the lockfile —
`<lockfile>.intent` (the writer-preference entry gate), `<lockfile>.alloc` (a
brief internal slot-allocator mutex), and `<lockfile>.slots/NN` (the `N` slot
files). Like the lockfile, they are created on demand and never unlinked.

On Windows the locks are `LockFileEx`-based like the rest of the tool; the
concurrency proofs in the test suite are exercised on unix.

#### Weighted mode on an inherited descriptor

Weighted mode works in descriptor mode too, so a shell can hold tokens on
inherited fds — the same way `exec 9>lock; flock 9` holds a plain lock — and a
pool can mix wrap-mode and descriptor-mode participants freely (they coordinate
through the same `<lockfile>` and its siblings).

A **full-pool writer** (`--tokens N --pool N`) needs only the main fd, because
draining the pool *is* taking the main lock exclusive:

```bash
exec 9<>/tmp/box.lock
flock --tokens 16 --pool 16 --lock-fd 9 /tmp/box.lock || exit 1
# ... exclusive critical section; released when this shell exits or:
flock -u 9
```

A **weighted reader** (`--tokens K` with `K < N`) needs `K` counted tokens, and
each token is a lock on a distinct slot file that must outlive `flock`. Since a
helper process cannot hand descriptors back to its parent, *the caller* pre-opens
all `N` slot files as inherited fds; `flock` locks `K` free ones (the locks ride
your fds past its exit) and prints the fd numbers it took:

```bash
lock=/tmp/box.lock
exec 9<>"$lock"                 # fd 9: the main lock
mkdir -p "$lock.slots"
for i in $(seq 0 15); do        # fds 10..25: the 16 slot files, in order
  printf -v slot '%02d' "$i"    # (bash; POSIX sh: slot=$(printf '%02d' "$i"))
  eval "exec $((10 + i))<>\"$lock.slots/$slot\""
done

# Take 5 of 16 tokens. Prints the slot fds it locked, e.g. "10 11 12 13 14".
flock --tokens 5 --pool 16 --lock-fd 9 --slot-fds 10-25 "$lock" || exit 1

# ... shared critical section ...

# Release by closing the fds (shell exit / SIGKILL) or explicitly per fd:
for fd in 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25; do flock -u "$fd"; done
```

`--slot-fds` takes an inclusive range (`10-25`) or a comma list (`10,11,12`),
listing exactly `--pool` descriptors in slot order (fd for slot `00`, slot `01`,
…). Leaving the unused slot fds open is harmless — only a *locked* slot counts
against the pool.

`flock` itself takes any fd number, but *your shell* may not: POSIX only
requires redirections on fds 0-9, and strict shells hold to it — under dash
(`/bin/sh` on Debian and Ubuntu) `exec 10<>f` is an error, since `10` parses as
a command name. The example above is `#!/bin/bash`; for a pool that must run
under `/bin/sh`, keep every fd single-digit (up to 7 slots alongside the main
fd) or re-exec under bash.

**Acquisition is all-or-nothing.** On a refused `-n`/`-w`, `flock` releases
every lock it took while trying — including the main-shared lock and any slot it
had already swept — so a failed attempt leaves *none* of your descriptors
locked. You never have to unwind a partial acquisition yourself; closing the fds
on failure remains fine hygiene but is not required for correctness.

**Descriptor/wrap equivalence.**

It's possible to do everything with either the inhereted FD or wrap a command mode.
The only asymmetry is the extra plumbing the fd form needs, and it
is there for a verified reason:

| Feature | Descriptor mode | Basis |
|---|---|---|
| `-s` / `-x` / `-n` / `-w` / `-u` | ✅ | Same `flock(2)` call on the inherited fd. |
| Weighted full pool (`K == N`) | `--lock-fd` | The inherited main fd carries the exclusive lock; the writer-preference gate is held only while waiting. |
| Weighted reader (`K < N`) | `--lock-fd` + `--slot-fds` (caller pre-opens `N` slot fds) | `K` tokens need `K` descriptors that outlive `flock`; a helper can't install fds into its parent, so the caller opens them. |


### Options

| Flag | Description |
|------|-------------|
| `-n`, `--nonblock` | Fail immediately (exit with the conflict code) if the lock is held, instead of waiting. |
| `-w`, `--timeout <SECONDS>` | Wait at most `SECONDS` (fractional allowed) for the lock, then fail with the conflict code. |
| `-s`, `--shared` | Take a shared lock instead of an exclusive one. |
| `-x`, `--exclusive` | Take an exclusive lock (the default; accepted for `flock(1)` parity). |
| `--tokens <K>` | Weighted mode: take `K` tokens from the `--pool`. `K == N` is exclusive. Requires `--pool`. Works in wrap mode (`<lockfile> <command>`) and descriptor mode (with `--lock-fd`). |
| `--pool <N>` | Weighted-mode pool size. All callers sharing a lockfile must pass the same value. Requires `--tokens`. |
| `--lock-fd <FD>` | Descriptor-mode weighted acquire: the caller's inherited fd on the lockfile itself. Requires `--tokens`. |
| `--slot-fds <SPEC>` | Descriptor-mode weighted reader (`K < N`): the caller's inherited fds on the `N` slot files, in slot order — an inclusive range `FIRST-LAST` or a comma list. Requires `--lock-fd`. |
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
mise run test-linux  # full suite on Linux, in Docker (see Dockerfile.test)
```

The integration suite spawns real `flock` child processes to prove the
mutual-exclusion and death-release properties end-to-end.

#### The tests are not portable for free

They drive real shells and real process groups, and both of those differ
between a developer mac and CI. A green run on macOS says nothing about Linux,
which is what `mise run test-linux` is for — run it before pushing anything
that touches the harness. Two differences have already cost a red CI:

- **`/bin/sh` is dash on Debian and Ubuntu, bash on macOS.** POSIX only requires
  redirections on fds 0-9, and dash holds to it: `exec 10<>f` is an error there,
  because `10` parses as a command name. Harness scripts that open descriptors
  must stay single-digit — see `SLOT_FD0` in `tests/fd_weighted.rs`.
- **`kill(1)` is not one program.** BSD `kill` (macOS) treats `-KILL -<pgid>` as
  a process group; procps-ng `kill` (Debian/Ubuntu) exits 1 and leaves the group
  running, so holders outlive their kill and tests stall for the holder's whole
  `sleep`. Signal process groups with `libc::killpg` — a dev-dependency for
  exactly this — never by shelling out. Reap once, too: after a `Child` is
  waited on, its pid can be reused, so a second `killpg` on that number can hit
  an unrelated group.

### Performing a release

Ensure git-cliff and cargo-release are both installed (run `mise install` to
get them) and run `cargo release [patch|minor]`.

If things look good, run again with `--execute`. Pushing the resulting `vX.Y.Z`
tag triggers the `dist` release workflow, which builds the per-platform
binaries and installers and publishes them to a GitHub Release.
