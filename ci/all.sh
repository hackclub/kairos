#!/usr/bin/env bash
# Runs every line of versions.txt. Usage: ci/all.sh <target> [lab flags]
set -u
target=$1
shift
exe=""
case "$target" in windows-*) exe=.exe ;; esac
failed=0
# In CI, outside the lab's checkout: Kairos takes anything inside a git repo for that project (hackclub/kairos).
work=work
if [ -n "${RUNNER_TEMP:-}" ]; then
  work=$RUNNER_TEMP/work
  command -v cygpath > /dev/null && work=$(cygpath -m "$RUNNER_TEMP")/work
fi
# Inside work/tools, so each run's downloads go away with it.
export npm_config_cache="$work/tools/.npm"
: > result.txt
# versions.txt comes in on fd 3: stdin stays the terminal, which the two-window runs read.
while read -r harness version scenario <&3; do
  [ -z "$harness" ] && continue
  # ONLY in lab.yml picks some lines, for trying one thing.
  [ -n "${ONLY:-}" ] && ! grep -qE "$ONLY" <<< "$harness $version $scenario" && continue
  "./target/release/lab$exe" run "$harness" "$version" --kairosd "dist/$target/kairosd$exe" --scenario "${scenario:-basic}" --work "$work" "$@" > out.txt 2>&1 || failed=1
  if grep -E "^(PASS|FAIL|SKIP|lab:|  )" out.txt > part.txt; then cat part.txt; else tail -20 out.txt | tee part.txt; fi
  cat part.txt >> result.txt
  rm -rf "$work/tools"
done 3< <(tr -d '\r' < "${VERSIONS:-versions.txt}")
echo "$failed" > failed.txt
exit "$failed"
