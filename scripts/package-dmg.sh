#!/usr/bin/env bash
# Build AgentsManager.app (Apple Silicon only) and wrap it in a .dmg.
#
# No Apple Developer account is involved: the bundle is ad-hoc signed, so whoever
# receives the .dmg has to clear the quarantine flag once (see docs/PACKAGING.md).
#
#   ./scripts/package-dmg.sh            # full build
#   SKIP_WEB=1 ./scripts/package-dmg.sh # reuse an existing web/dist
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET=aarch64-apple-darwin
PRODUCT=AgentsManager
BUNDLE="target/$TARGET/release/bundle/macos/$PRODUCT.app"
OUT_DIR="$ROOT/dist"

step() { printf '\n\033[1;35m==>\033[0m %s\n' "$1"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

# ---------------------------------------------------------------- 0. preflight
step "preflight"
[ "$(uname -s)" = Darwin ] || die "macOS only"
# The bundle is arm64-only, and an Intel host would build one it cannot launch.
[ "$(uname -m)" = arm64 ] || die "Apple Silicon only — this host is $(uname -m); see docs/PACKAGING.md"
command -v cargo  >/dev/null || die "cargo not found — install Rust from https://rustup.rs"
# A Homebrew Rust has cargo but no rustup, and the target below is added with rustup.
command -v rustup >/dev/null || die "rustup not found — the $TARGET target needs it; install Rust from https://rustup.rs"
command -v bun    >/dev/null || die "bun not found — install from https://bun.sh"
xcode-select -p >/dev/null 2>&1 || die "Xcode command line tools missing — run: xcode-select --install"
rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"

VERSION="$(python3 -c 'import json,sys;print(json.load(open("desktop/tauri.conf.json"))["version"])')"
DMG="$OUT_DIR/$PRODUCT-$VERSION-arm64.dmg"

# ---------------------------------------------------------------- 1. web UI
# The daemon embeds web/dist with rust-embed (daemon/src/assets.rs), so this must
# run before the cargo build, not after.
if [ "${SKIP_WEB:-}" = 1 ] && [ -f web/dist/index.html ]; then
  step "web ui (skipped, reusing web/dist)"
else
  step "web ui"
  ( cd web && bun install --frozen-lockfile && bun run build )
fi
[ -f web/dist/index.html ] || die "web/dist/index.html missing after the frontend build"

# ---------------------------------------------------------------- 2. daemon
step "daemon (agents-managerd, $TARGET)"
cargo build --release -p agents-managerd --target "$TARGET"

# Tauri looks for `<externalBin>-<triple>` and drops the suffix inside the bundle.
mkdir -p desktop/binaries
cp -f "target/$TARGET/release/agents-managerd" "desktop/binaries/agents-managerd-$TARGET"

# ---------------------------------------------------------------- 3. .app
step "desktop shell (tauri)"
( cd desktop && { [ -d node_modules ] || bun install --frozen-lockfile; } \
  && bunx --no-install tauri build --target "$TARGET" --bundles app )
[ -d "$BUNDLE" ] || die "expected $BUNDLE"

# ---------------------------------------------------------------- 4. ad-hoc sign
# Without a Developer ID the best we can do is an ad-hoc signature. It still has to be
# there: an unsigned arm64 binary will not launch at all on Apple Silicon.
step "ad-hoc sign"
codesign --force --sign - --timestamp=none "$BUNDLE/Contents/MacOS/agents-managerd"
codesign --force --sign - --timestamp=none "$BUNDLE/Contents/MacOS/$PRODUCT"
codesign --force --sign - --timestamp=none "$BUNDLE"
codesign --verify --deep --strict "$BUNDLE"

# ---------------------------------------------------------------- 5. .dmg
step "dmg"
mkdir -p "$OUT_DIR"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$BUNDLE" "$STAGE/$PRODUCT.app"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "$PRODUCT" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null

printf '\n\033[1;32mdone\033[0m  %s  (%s)\n' "$DMG" "$(du -h "$DMG" | cut -f1)"
printf '安裝後首次開啟請執行：xattr -dr com.apple.quarantine "/Applications/%s.app"\n' "$PRODUCT"
