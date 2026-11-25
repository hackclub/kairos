#!/usr/bin/env bash
# Runs every line of versions.txt. Usage: ci/all.sh <target> [lab flags]
set -u
target=$1
shift
exe=""
case "$target" in windows-*) exe=.exe ;; esac
failed=0
# Inside work/tools, so each run's downloads go away with it.
export npm_config_cache="$PWD/work/tools/.npm"
: > result.txt
while read -r harness version; do
  [ -z "$harness" ] && continue
  # ONLY in lab.yml picks some lines, for trying one thing.
  [ -n "${ONLY:-}" ] && ! grep -qE "$ONLY" <<< "$harness $version" && continue
  "./target/release/lab$exe" run "$harness" "$version" --kairosd "dist/$target/kairosd$exe" "$@" > out.txt 2>&1 || failed=1
  if grep -E "^(PASS|FAIL|SKIP|lab:|  )" out.txt > part.txt; then cat part.txt; else tail -20 out.txt | tee part.txt; fi
  cat part.txt >> result.txt
  rm -rf work/tools
done < <(tr -d '\r' < versions.txt)
echo "$failed" > failed.txt
exit "$failed"
