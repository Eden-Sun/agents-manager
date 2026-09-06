#!/usr/bin/env bash
# Regenerate desktop/icons/icon.icns from web/public/favicon.svg.
# Only needed when the mark changes — the .icns is committed, so packaging never runs this.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Compose a 1024px artboard: rounded plate + the favicon mark, centred.
python3 - "$ROOT/web/public/favicon.svg" "$WORK/icon.svg" <<'PY'
import re, sys
src, dst = sys.argv[1], sys.argv[2]
svg = open(src).read()
inner = re.sub(r'(?s)^.*?<svg[^>]*>', '', svg).rsplit('</svg>', 1)[0]
W = 430.0                       # mark width on a 1024 artboard
s = W / 48.0                    # favicon viewBox is 0 0 48 46
x, y = (1024 - W) / 2, (1024 - 46 * s) / 2
open(dst, 'w').write(f'''<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
  <defs>
    <linearGradient id="plate" x1="0" y1="0" x2="0.4" y2="1">
      <stop offset="0" stop-color="#f7f4ff"/>
      <stop offset="0.55" stop-color="#e6dcff"/>
      <stop offset="1" stop-color="#d3c2ff"/>
    </linearGradient>
  </defs>
  <rect x="82" y="82" width="860" height="860" rx="200" ry="200" fill="url(#plate)"/>
  <g transform="translate({x:.2f} {y:.2f}) scale({s:.5f})">{inner}</g>
</svg>
''')
PY

qlmanage -t -s 1024 -o "$WORK" "$WORK/icon.svg" >/dev/null 2>&1 || true
PNG="$WORK/icon.svg.png"
[ -f "$PNG" ] || { echo "Quick Look could not render the SVG; render $WORK/icon.svg to 1024px PNG by hand" >&2; exit 1; }

SET="$WORK/icon.iconset"; mkdir -p "$SET"
for s in 16 32 128 256 512; do
  sips -z "$s"          "$s"          "$PNG" --out "$SET/icon_${s}x${s}.png"    >/dev/null
  sips -z "$((s*2))" "$((s*2))"       "$PNG" --out "$SET/icon_${s}x${s}@2x.png" >/dev/null
done
mkdir -p "$ROOT/desktop/icons"
iconutil -c icns "$SET" -o "$ROOT/desktop/icons/icon.icns"
cp "$PNG" "$ROOT/desktop/icons/icon.png"
echo "wrote desktop/icons/icon.icns"
