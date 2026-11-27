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
