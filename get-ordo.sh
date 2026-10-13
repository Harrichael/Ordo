#!/usr/bin/env bash
# Builds Ordo from source and installs it as a signed Ordo.app:
#
#     curl -fsSL https://raw.githubusercontent.com/Harrichael/Ordo/main/get-ordo.sh | bash
#
# Run it again to update. The machine needs Xcode's command line tools and a
# Rust toolchain; this script says the command for whichever is missing rather
# than installing it. It does not start Ordo or set it to start at login.
#
# Knobs, all environment: ORDO_APP (default ~/Applications/Ordo.app),
# INSTALL_DIR (where the `ordo` command is linked, default ~/.local/bin),
# ORDO_SRC (where the checkout lives), ORDO_REPO, ORDO_REF.
set -euo pipefail

REPO="${ORDO_REPO:-https://github.com/Harrichael/Ordo}"
REF="${ORDO_REF:-main}"
SRC="${ORDO_SRC:-${XDG_CACHE_HOME:-$HOME/.cache}/ordo/src}"
APP="${ORDO_APP:-$HOME/Applications/Ordo.app}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

BUNDLE_ID="com.harrichael.ordo"
CERT_NAME="Ordo Local Code Signing"
STATE_DIR="$HOME/Library/Application Support/Ordo"
KEYCHAIN="$STATE_DIR/signing.keychain-db"

case "${1:-}" in
  -h|--help)
    if [ -r "${BASH_SOURCE[0]:-}" ]; then
      sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    else
      echo "usage: get-ordo.sh   # builds and installs Ordo.app from source"
      echo "env: ORDO_APP INSTALL_DIR ORDO_SRC ORDO_REPO ORDO_REF"
    fi
    exit 0
    ;;
  "") ;;
  *) echo "error: unknown argument: $1 (try --help)" >&2; exit 2 ;;
esac

have() { command -v "$1" >/dev/null 2>&1; }
say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = Darwin ] || die "Ordo is a macOS window manager; this is $(uname -s)."
# /usr/bin/cc and /usr/bin/git exist even without the command line tools, as
# stubs that pop an install dialog, so asking for them proves nothing.
xcode-select -p >/dev/null 2>&1 || die "Xcode command line tools missing. run: xcode-select --install"
have codesign || die "codesign not found. run: xcode-select --install"
PATH="$HOME/.cargo/bin:$PATH"
have cargo || die "cargo not found. install Rust: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"

# --- the source -----------------------------------------------------------

# A checkout someone already has is theirs, and is built exactly as it stands:
# named in ORDO_SRC, or the clone this script is sitting in. Only the cache
# directory is ours to fetch into and reset.
HERE=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]:-}" ]; then
  HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi
if [ -n "${ORDO_SRC:-}" ]; then
  [ -f "$SRC/crates/ordo/Cargo.toml" ] || die "no Ordo checkout at $SRC"
  say "building $SRC"
elif [ -n "$HERE" ] && [ -f "$HERE/crates/ordo/Cargo.toml" ]; then
  SRC="$HERE"
  say "building the checkout this script came from: $SRC"
elif [ -d "$SRC/.git" ]; then
  say "updating $SRC"
  git -C "$SRC" fetch --quiet --depth 1 origin "$REF"
  git -C "$SRC" reset --quiet --hard FETCH_HEAD
else
  say "cloning $REPO into $SRC"
  mkdir -p "$(dirname "$SRC")"
  git clone --quiet --depth 1 --branch "$REF" "$REPO" "$SRC"
fi

cargo build --release --quiet -p ordo --manifest-path "$SRC/Cargo.toml"
BIN="${CARGO_TARGET_DIR:-$SRC/target}/release/ordo"
[ -x "$BIN" ] || die "the build finished but $BIN is not there"

# --- the signing identity -------------------------------------------------

# macOS ties an Accessibility grant to the app's signature. A plain cargo build
# is signed ad hoc, by its own hash, so every rebuild would be a stranger to
# the grant and Ordo would silently lose its event tap. Signing every build
# with one local certificate keeps the grant across updates. The certificate
# needs no trust settings; codesign only has to find it on the keychain search
# list, so the keychain joins the list for the signing and leaves after.
#
# The keychain has no password. Anything running as this user can read it and
# sign code that macOS would take for Ordo. That is the price of an install
# that asks for nothing; the same user's processes already reach most of what
# Ordo can.
mkdir -p "$STATE_DIR"
if [ ! -f "$KEYCHAIN" ]; then
  say "creating a local signing certificate in $KEYCHAIN"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  cat >"$tmp/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $CERT_NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF
  # macOS's own LibreSSL, not a Homebrew OpenSSL 3: `security import` cannot
  # read the PKCS#12 encryption OpenSSL 3 writes by default.
  /usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 7300 \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" -config "$tmp/cert.cnf" 2>/dev/null ||
    die "/usr/bin/openssl could not make the certificate"
  /usr/bin/openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
    -out "$tmp/id.p12" -passout pass:ordo ||
    die "/usr/bin/openssl could not package the certificate"
  security create-keychain -p "" "$KEYCHAIN"
  security set-keychain-settings "$KEYCHAIN" # no auto-lock
  security unlock-keychain -p "" "$KEYCHAIN"
  security import "$tmp/id.p12" -k "$KEYCHAIN" -P ordo -T /usr/bin/codesign >/dev/null
  security set-key-partition-list -S apple-tool:,apple: -s -k "" "$KEYCHAIN" >/dev/null
  chmod 600 "$KEYCHAIN"
fi
security unlock-keychain -p "" "$KEYCHAIN"
IDENTITY="$(security find-identity -p codesigning "$KEYCHAIN" | awk -v n="\"$CERT_NAME\"" '$0 ~ n {print $2; exit}')"
[ -n "$IDENTITY" ] || die "no \"$CERT_NAME\" identity in $KEYCHAIN; delete that file and run again"

# --- the app --------------------------------------------------------------

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$SRC/crates/ordo/Cargo.toml" | head -1)"
revision="$(git -C "$SRC" rev-parse --short HEAD 2>/dev/null || echo unknown)"

say "assembling Ordo.app $version ($revision)"
stage="$APP.new"
rm -rf "$stage"
mkdir -p "$stage/Contents/MacOS"
cp "$BIN" "$stage/Contents/MacOS/ordo"
cat >"$stage/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>ordo</string>
  <key>CFBundleName</key><string>Ordo</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>OrdoGitRevision</key><string>$revision</string>
  <key>LSUIElement</key><true/>
</dict>
</plist>
EOF

search_list=()
while IFS= read -r k; do search_list+=("$k"); done < <(security list-keychains -d user | sed 's/^ *"//; s/"$//')
restore_search_list() { security list-keychains -d user -s "${search_list[@]}"; }
trap 'restore_search_list; [ -n "${tmp:-}" ] && rm -rf "$tmp"' EXIT
security list-keychains -d user -s "${search_list[@]}" "$KEYCHAIN"
signed="$(codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$stage" 2>&1)" ||
  die "codesign failed: $signed"
restore_search_list
codesign --verify --strict "$stage" || die "Ordo.app did not verify after signing"

# A running Ordo keeps the binary it started with, so swapping the bundle under
# it is safe; it picks up this build at its next start.
mkdir -p "$(dirname "$APP")"
rm -rf "$APP.old"
[ -e "$APP" ] && mv "$APP" "$APP.old"
mv "$stage" "$APP"
rm -rf "$APP.old"

mkdir -p "$INSTALL_DIR"
ln -sf "$APP/Contents/MacOS/ordo" "$INSTALL_DIR/ordo"

# --- what is left for a human ---------------------------------------------

running=""
pid="$(cat "$STATE_DIR/ordo.pid" 2>/dev/null || true)"
[ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && running="$pid"

cat <<EOF

installed $APP ($version, $revision), and linked $INSTALL_DIR/ordo to it.

one manual step, once per machine:
  System Settings > Privacy & Security > Accessibility: add $APP
  Updates through this script keep the grant; without it Ordo logs
  "could not create event tap" and does nothing.

to run it:
  $APP/Contents/MacOS/ordo run            engaged at launch
  $APP/Contents/MacOS/ordo run --paused   waits for Ctrl+Alt+Cmd+O
  ordo rescue                             the kill switch, from any terminal
EOF
if [ -n "$running" ]; then
  echo
  echo "Ordo is running as pid $running on its old binary; restart it to pick up this build."
fi
