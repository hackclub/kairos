set -euo pipefail
. "$(dirname "$0")/lib.sh"

export XDG_RUNTIME_DIR=/tmp/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"

start_sway() {
  WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman sway >>/tmp/sway.log 2>&1 &
  sway_pid=$!
  wait_for test -S "$XDG_RUNTIME_DIR/wayland-1"
  wait_for test -S "$XDG_RUNTIME_DIR/sway-ipc.$(id -u).$sway_pid.sock"
}

start_sway
export WAYLAND_DISPLAY=wayland-1
# Exported the way systemd hands kairosd the login's environment, so after the restart it names a socket that is gone.
export SWAYSOCK="$XDG_RUNTIME_DIR/sway-ipc.$(id -u).$sway_pid.sock"

check supervisor_finds_the_compositor_again_after_it_restarts &
test_run=$!
sleep 15
kill "$sway_pid"
wait "$sway_pid" || true
start_sway
wait "$test_run"
