#!/bin/sh
# Smallest release build: nightly std rebuilt for size, immediate-abort panics,
# no unwind tables, lld with identical-code folding, packed relative relocations
# (RELR: keeps PIE/ASLR, needs glibc >= 2.36) and LLVM's machine outliner (shares
# repeated instruction sequences). Output: dist/maia
set -e
cd "$(dirname "$0")"
export RUSTFLAGS="-Zunstable-options -Cpanic=immediate-abort -Zlocation-detail=none -Zfmt-debug=none \
 -Cforce-unwind-tables=no -Clink-arg=-fuse-ld=lld -Clink-arg=-Wl,--icf=all \
 -Clink-arg=-Wl,--gc-sections -Clink-arg=-Wl,-O2 -Clink-arg=-Wl,-z,pack-relative-relocs \
 -Cllvm-args=-enable-machine-outliner=always ${EXTRA_RUSTFLAGS}"
cargo +nightly build --release -Z build-std=std,panic_abort --target x86_64-unknown-linux-gnu
mkdir -p dist
# Replace atomically so a running copy keeps working.
cp target/x86_64-unknown-linux-gnu/release/maia dist/maia.new && mv -f dist/maia.new dist/maia
ls -l dist/maia | awk '{print "dist/maia:", $5, "bytes"}'
# Keep an installed copy (maia --install) on the latest build, also atomically.
I="${XDG_BIN_HOME:-$HOME/.local/bin}/maia"
if [ -f "$I" ] && [ -f "${XDG_DATA_HOME:-$HOME/.local/share}/applications/io.github.yappologistic.Maia.desktop" ]; then
  cp dist/maia "$I.new" && mv -f "$I.new" "$I" && echo "updated $I"
fi
