#!/usr/bin/env bash
# Render docs/SHORTCUTS.md to a PDF, and optionally print it.
#
#   scripts/shortcuts-pdf.sh [--print] [--printer NAME]
#
# Uses pandoc if available (nicest output); otherwise falls back to a built-in
# Markdown -> aligned-plain-text pass (python3) piped through cupsfilter, so it
# works on a stock macOS with no extra installs.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
md="$repo_root/docs/SHORTCUTS.md"
pdf="$repo_root/docs/SHORTCUTS.pdf"

do_print=0
printer=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --print) do_print=1 ;;
    --printer) printer="$2"; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
  shift
done

if [[ ! -f "$md" ]]; then
  echo "not found: $md" >&2
  exit 1
fi

if command -v pandoc >/dev/null 2>&1; then
  pandoc "$md" -o "$pdf"
else
  # Fallback: format Markdown to clean monospaced text, then let CUPS make the
  # PDF (cupsfilter renders text/plain but not HTML on modern macOS).
  txt="$(mktemp -t shortcuts).txt"
  python3 "$repo_root/scripts/md_to_text.py" "$md" > "$txt"
  cupsfilter "$txt" > "$pdf" 2>/dev/null
  rm -f "$txt"
fi

echo "wrote $pdf"

if [[ "$do_print" -eq 1 ]]; then
  if [[ -n "$printer" ]]; then
    lp -d "$printer" "$pdf"
  else
    lp "$pdf" # system default printer
  fi
  echo "sent to printer${printer:+ $printer}"
fi
