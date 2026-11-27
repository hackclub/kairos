#!/bin/bash
# What's on a Mac's screen between lab runs: the windows front first, Terminal's windows (1 is in front) with the
# device each shows, and the processes on a terminal device.
"$1"
perl -e 'alarm 10; exec @ARGV' osascript -e '
tell application "Terminal"
  set out to ""
  repeat with w in windows
    set t to selected tab of w
    set out to out & "terminal window " & (index of w) & " id " & (id of w) & " " & (tty of t) & " busy " & (busy of t) & linefeed
  end repeat
  return out
end tell' 2>&1
ps -axo pid,ppid,tty,stat,comm | awk 'NR == 1 || $3 != "??"'
# Where a second window's lab process that outlived its run is waiting.
for pid in $(pgrep -f "release/lab pane"); do
  echo "lab pane $pid still running, its processes:"
  ps -axo pid,ppid,pgid,stat,tty,etime,command | awk -v p="$pid" 'NR == 1 || $1 == p || $2 == p || $3 == p' | cut -c1-200
  for kid in $(pgrep -P "$pid"); do
    echo "child $kid:"
    ps -axo pid,ppid,pgid,stat,tty,command | awk -v p="$kid" '$2 == p || $3 == p' | cut -c1-200
    sample "$kid" 1 2>/dev/null | awk '/Call graph:/{p=1} /Total number in stack/{p=0} p' | grep -vE "^\s*$" | cut -c1-160 | head -30
  done
  sample "$pid" 1 2>/dev/null | awk '/Call graph:/{p=1} /Total number in stack/{p=0} p' | grep -vE "^\s*$" | cut -c1-160 | head -60
done
