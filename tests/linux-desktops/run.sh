#!/usr/bin/env bash
#   tests/linux-desktops/run.sh [x11 | x11-late | sway | sway-restart | dbus | all]
# Needs docker and cargo.
set -euo pipefail
cd "$(dirname "$0")/../.."

image=${KAIROS_DESKTOP_IMAGE:-kairos-desktops}
scenarios=${1:-all}
[ "$scenarios" = all ] && scenarios="x11 x11-late sway sway-restart dbus"

docker image inspect "$image" >/dev/null 2>&1 || docker build -q -t "$image" tests/linux-desktops

bin=$(cargo test -p kairos-agent --no-run 2>&1 | sed -n 's/^ *Executable unittests src\/main.rs (\(.*\))$/\1/p')
[ -x "$bin" ] || { echo "test binary not found" >&2; exit 1; }
bin=$(realpath "$bin")

status=0
for s in $scenarios; do
  echo "== $s"
  docker run --rm --name "kairos-desktop-$s" --network none --memory 1g --cpus 2 \
    -v "$bin:/kairosd-test:ro" -v "$PWD:/repo:ro" "$image" bash "/repo/tests/linux-desktops/$s.sh" || status=1
done
exit $status
