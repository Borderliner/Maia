#!/bin/sh
# A real Maia window for checking changes on the desktop, sandboxed: empty home, no
# network, only target/window writable. Input comes through a FIFO read by the testkit
# feature (src/testkit.rs), so your own mouse and keyboard are never used.
#
#   tools/window.sh build           testkit release build into target-tk/
#   tools/window.sh start [PATH]    open PATH (default: target/window/files)
#   tools/window.sh send CMD...     move/click/dclick/drag/down/up/key/text/scroll X Y ...
#   tools/window.sh shot NAME       target/window/shots/NAME.png (and NAME.s.png at 50%)
#   tools/window.sh stop            kill the test window, by PID
#
# On Hyprland, MAIA_WS=N opens the window on workspace N (a window rule for the test
# build's class, moved there again in case the rule did not apply); a headless output
# (hyprctl output create headless) keeps it off the screen someone is using.
set -e
C=$(dirname "$(dirname "$(readlink -f "$0")")")
W=$C/target/window
FIFO=$W/input.fifo
BIN=${MAIA_BIN:-$C/target-tk/release/maia}

case "$1" in
build)
  cd "$C" && cargo build --release --features testkit --target-dir target-tk
  ;;
start)
  mkdir -p "$W/home" "$W/files" "$W/shots"
  [ -p "$FIFO" ] || mkfifo "$FIFO"
  WL=${WAYLAND_DISPLAY:-wayland-1}
  GPU=""; for d in /dev/dri /dev/nvidia*; do [ -e "$d" ] && GPU="$GPU --dev-bind $d $d"; done
  # Test builds have their own window class, so a rule can place them before they show.
  clients() { hyprctl -j clients 2>/dev/null | python3 -c 'import json,sys; [print(c["address"]) for c in json.load(sys.stdin) if c["class"]=="io.github.yappologistic.Maia.test"]' 2>/dev/null; }
  BEFORE=$(clients)
  if [ -n "$MAIA_WS" ]; then
    hyprctl eval "hl.window_rule({ match = { class = \"^(io\\\\.github\\\\.yappologistic\\\\.Maia\\\\.test)\$\" }, workspace = \"$MAIA_WS silent\" })" > /dev/null 2>&1 ||
      hyprctl keyword windowrule "workspace $MAIA_WS silent, class:^(io\\.github\\.yappologistic\\.Maia\\.test)\$" > /dev/null 2>&1
  fi
  # shellcheck disable=SC2086
  (setsid bwrap --ro-bind /usr /usr --symlink usr/lib /lib --symlink usr/lib64 /lib64 \
    --symlink usr/bin /bin --symlink usr/bin /sbin --ro-bind /etc /etc --ro-bind /sys /sys \
    --proc /proc --dev /dev $GPU --tmpfs /tmp --tmpfs /home --bind "$W" "$W" \
    --ro-bind "$(dirname "$BIN")" "$(dirname "$BIN")" \
    --dir "$XDG_RUNTIME_DIR" --ro-bind "$XDG_RUNTIME_DIR/$WL" "$XDG_RUNTIME_DIR/$WL" \
    --unshare-all \
    --setenv HOME "$W/home" --setenv XDG_RUNTIME_DIR "$XDG_RUNTIME_DIR" --setenv WAYLAND_DISPLAY "$WL" \
    --setenv COMET_TEST_FIFO "$FIFO" ${MAIA_PATH:+--setenv PATH "$MAIA_PATH:/usr/bin"} \
    "$BIN" "${2:-$W/files}" > "$W/maia.log" 2>&1 & echo $! > "$W/pid")
  [ -n "$MAIA_WS" ] || exit 0
  # Move only the new window, never one that was already open.
  for _ in $(seq 1 600); do
    sleep 0.05
    A=$(clients | grep -vxF "$BEFORE" | head -1 || true)
    [ -n "$A" ] && break
  done
  if [ -z "$A" ]; then echo "no window after 30 s; stopping it" >&2; "$0" stop; exit 1; fi
  # Lua-configured Hyprland first, then the classic dispatcher.
  R=$(hyprctl dispatch "hl.dsp.window.move({ window = 'address:$A', workspace = '$MAIA_WS', follow = false })")
  [ "$R" = ok ] || hyprctl dispatch movetoworkspacesilent "$MAIA_WS,address:$A" > /dev/null
  ;;
send)
  shift; echo "$*" > "$FIFO"
  ;;
shot)
  P=$W/shots/$2.png; rm -f "$P"; echo "shot $P" > "$FIFO"
  for _ in $(seq 1 40); do [ -s "$P" ] && break; sleep 0.1; done; sleep 0.2
  magick "$P" -resize 50% "$W/shots/$2.s.png"
  ;;
stop)
  [ -f "$W/pid" ] || exit 0
  P=$(cat "$W/pid"); K="$P $(pgrep -P "$P" || true)"
  for c in $(pgrep -P "$P" || true); do K="$K $(pgrep -P "$c" || true)"; done
  # shellcheck disable=SC2086
  kill $K 2>/dev/null || true; rm -f "$W/pid"
  ;;
*)
  sed -n '2,13p' "$0"; exit 1
  ;;
esac
