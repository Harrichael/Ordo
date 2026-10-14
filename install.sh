#!/usr/bin/env bash
# Builds and installs this checkout as get-ordo.sh does: a signed
# ~/Applications/Ordo.app and the ~/.local/bin/ordo link. Never starts or
# restarts Ordo.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ORDO_SRC="$here" exec "$here/get-ordo.sh" "$@"
