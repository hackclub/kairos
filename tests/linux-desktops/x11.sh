set -euo pipefail
. "$(dirname "$0")/lib.sh"

export DISPLAY=:99
Xvfb :99 -screen 0 1280x800x24 >/dev/null 2>&1 &
wait_for xdotool getdisplaygeometry

check x11_requires_a_window_manager
openbox >/dev/null 2>&1 &
wait_for sh -c 'xprop -root _NET_SUPPORTING_WM_CHECK | grep -q "window id"'
check x11_focus_follows_the_active_window
check x11_idle_follows_input
check x11_idle_tells_xtest_input_from_key_presses
check supervisor_selects_the_desktops_backends KAIROS_EXPECT_BACKENDS=x11,x11-screensaver

# A daemon started by systemd has none of the session variables and finds them in other processes.
xterm -e sleep 600 &
wait_for xdotool search --onlyvisible --class xterm
check session_is_found_from_other_processes -i HOME=/root
