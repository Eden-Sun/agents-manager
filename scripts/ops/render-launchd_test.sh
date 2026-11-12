#!/bin/bash
# render-launchd.sh 的隔離測試：樣板的 `@HOME@` 換成 ${HOME}（XML 轉義），結果能被 plistlib 解析、路徑正確。
#   bash scripts/ops/render-launchd_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
RENDER="$HERE/render-launchd.sh"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/render-launchd.XXXXXX")
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0
ok() { PASS=$((PASS + 1)); echo "ok   $1"; }
bad() { FAIL=$((FAIL + 1)); echo "FAIL $1"; }

cat > "$TMP/t.plist" <<'PL'
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>ProgramArguments</key><array><string>@HOME@/bin/x.sh</string></array></dict></plist>
PL

HOME="/home/tester" bash "$RENDER" "$TMP/t.plist" "$TMP/out.plist" && \
  grep -q '<string>/home/tester/bin/x.sh</string>' "$TMP/out.plist" && ok "換成 \$HOME" || bad "換成 \$HOME"
grep -q '@HOME@' "$TMP/out.plist" && bad "佔位沒換乾淨" || ok "佔位全部換掉"

HOME="/home/a&b<c" bash "$RENDER" "$TMP/t.plist" "$TMP/esc.plist" && \
  python3 -I -c 'import plistlib,sys; d=plistlib.load(open(sys.argv[1],"rb")); assert d["ProgramArguments"][0]=="/home/a&b<c/bin/x.sh", d' "$TMP/esc.plist" && \
  ok "含 & 與 < 的家目錄轉義正確" || bad "含 & 與 < 的家目錄轉義正確"

HOME="/home/tester" bash "$RENDER" "$TMP/t.plist" > "$TMP/stdout.plist" && grep -q '/home/tester/bin' "$TMP/stdout.plist" && ok "沒給輸出檔時寫到 stdout" || bad "stdout 模式"

for f in "$HERE"/launchd/*.plist; do
  HOME="/home/tester" bash "$RENDER" "$f" "$TMP/r.plist" && python3 -I -c 'import plistlib,sys; plistlib.load(open(sys.argv[1],"rb"))' "$TMP/r.plist" || { bad "$(basename "$f") 渲染後解析失敗"; continue; }
done
ok "launchd 樣板全部渲染後可解析"

echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ]
