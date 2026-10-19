#!/bin/sh
# Run Maia's headless user simulations (src/sim.rs) in a sandbox: the whole filesystem
# read-only, /home and /run replaced by empty tmpfs, no network, only a fresh in-memory
# sim root writable. Usage: sim.sh [test-name-filter] [extra test args]
# MAIA_SIM_SHOTS=DIR (an existing folder) is also writable: sessions that call
# `Sim::shot` paint their frames there in software, as PNGs.
set -e
C=$(dirname "$(dirname "$(readlink -f "$0")")"); D=${SIM_DIR:-$C/target/simroot}
SIM=$D/root
cd "$C"
BIN=$(cargo test --profile ${SIM_PROFILE:-sim} --no-run 2>&1 | sed -n 's/.*Executable unittests src\/main.rs (\(.*\))/\1/p' | tail -1)
[ -n "$BIN" ] || { echo "no test binary" >&2; exit 1; }
chmod -R u+rwX "$SIM" 2>/dev/null || true; rm -rf "$SIM"; mkdir -p "$SIM"
F=${1:-sim::}; [ $# -gt 0 ] && shift
# Hard limits so a simulated user can never strain this machine: the writable sim
# root is a 1 GB in-memory folder (not the disk), the other scratch mounts are tiny,
# and the whole run lives in a transient systemd scope capped at 4 GB of memory. The
# /home skeleton is read-only: a session must not be able to rename the folders that
# lead to the sim root (it once moved the 1 GB mount out of reach). A machine with no
# user session to cap (a CI runner, thrown away after) runs it without the scope.
GB=1073741824
SCOPE="systemd-run --user --scope --quiet -p MemoryMax=4G -p MemorySwapMax=0"
systemctl --user show-environment >/dev/null 2>&1 || SCOPE=
exec $SCOPE \
  bwrap --ro-bind / / --dev /dev --proc /proc \
  --size 67108864 --tmpfs /tmp --size 1048576 --tmpfs /run --size 1048576 --tmpfs /home \
  --ro-bind "$C/target" "$C/target" --size $GB --tmpfs "$SIM" --remount-ro /home --remount-ro /run \
  --unshare-all --die-with-parent --new-session \
  --unsetenv WAYLAND_DISPLAY --unsetenv DISPLAY --unsetenv DBUS_SESSION_BUS_ADDRESS \
  ${MAIA_SIM_SHOTS:+--bind "$MAIA_SIM_SHOTS" "$MAIA_SIM_SHOTS" --setenv MAIA_SIM_SHOTS "$MAIA_SIM_SHOTS"} \
  --setenv SDL_AUDIODRIVER dummy ${MAIA_PROF:+--setenv MAIA_PROF 1} ${MAIA_SIM_DEBUG:+--setenv MAIA_SIM_DEBUG 1} --setenv MAIA_SIM_SANDBOX 1 --setenv MAIA_SIM_ROOT "$SIM" --setenv HOME "$SIM/home" \
  ${MAIA_SIM_SEED:+--setenv MAIA_SIM_SEED $MAIA_SIM_SEED} ${MAIA_SIM_LOOK:+--setenv MAIA_SIM_LOOK $MAIA_SIM_LOOK} ${MAIA_SIM_THEME:+--setenv MAIA_SIM_THEME $MAIA_SIM_THEME} --setenv MAIA_SIM_SEEDS "${MAIA_SIM_SEEDS:-10}" --setenv MAIA_SIM_STEPS "${MAIA_SIM_STEPS:-300}" \
  "$C/$BIN" "$F" --test-threads=1 --nocapture "$@"
