#!/usr/bin/env bash
# Copy the inputs `cargo llvm-cov report` needs into a staging directory,
# keeping their paths relative to target/llvm-cov-target/:
#
#   - every .profraw file
#   - every instrumented executable (test binaries and the flock binary
#     the integration tests run)
#
# Executables are found with `find`, not a hardcoded path, because Cargo
# moves them: up to Rust 1.99 they sat in debug/deps/, and from 1.100 the
# new build-dir layout puts them in debug/build/<pkg>/<hash>/out/.
# Build scripts are skipped; they aren't instrumented and aren't needed.
#
# Fails when either set is empty, so a layout change breaks this step
# rather than producing an artifact that silently holds only profraws.
set -euo pipefail

src=target/llvm-cov-target
dest=$(realpath -m "${1:?usage: stage-coverage.sh DEST}")

mkdir -p "$dest"
cd "$src"

profraws=$(find . -maxdepth 1 -type f -name '*.profraw' | wc -l)
# Executables only, no extension (skips .so, .d, .rlib, ...), no build scripts.
mapfile -t bins < <(find debug -type f -perm -u+x ! -name '*.*' \
  ! -name 'build_script_build*' ! -name 'build-script-build*' ! -path '*/incremental/*')

if [[ $profraws -eq 0 ]]; then
  echo "error: no .profraw files in $src" >&2
  exit 1
fi
if [[ ${#bins[@]} -eq 0 ]]; then
  echo "error: no instrumented executables under $src/debug" >&2
  exit 1
fi

find . -maxdepth 1 -type f -name '*.profraw' -exec cp --parents {} "$dest/" \;
cp --parents "${bins[@]}" "$dest/"

echo "staged $profraws profraw files and ${#bins[@]} executables:"
printf '  %s\n' "${bins[@]}"
