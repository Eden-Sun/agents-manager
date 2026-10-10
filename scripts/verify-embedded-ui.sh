#!/usr/bin/env bash
# 打包前確認 daemon 嵌進去的前端就是 web/dist 現在的內容（#1071）。
#
#   scripts/verify-embedded-ui.sh <agents-managerd> [web/dist]
#
# rust-embed 是把檔案原樣編進二進位，所以 `index.html` 的每個位元組、以及它引用的 `assets/` 檔，都必須原封不動
# 出現在 binary 裡。前端重建了但 binary 還是舊的（或 web/dist 缺檔、沒建過前端而嵌成 404）→ exit 1，
# 不讓 .dmg 帶著舊 UI 出去。
set -euo pipefail

BIN="${1:?usage: verify-embedded-ui.sh <agents-managerd> [web/dist]}"
DIST="${2:-web/dist}"
[ -f "$DIST/index.html" ] || { echo "error: $DIST/index.html 不存在，沒有可以比對的前端（先 bun run build）" >&2; exit 1; }
[ -f "$BIN" ] || { echo "error: 找不到 $BIN" >&2; exit 1; }

python3 - "$BIN" "$DIST" <<'PY'
import os, re, sys

binary, dist = sys.argv[1], sys.argv[2]
data = open(binary, "rb").read()
index = open(os.path.join(dist, "index.html"), "rb").read()
problems = []
if index not in data:
    problems.append("index.html 的內容不在 binary 裡：binary 是用別的（舊的）web/dist 編的")
refs = sorted(set(re.findall(rb'(?:src|href)="/?(assets/[^"]+)"', index)))
for ref in refs:
    name = ref.decode()
    path = os.path.join(dist, name)
    if not os.path.isfile(path):
        problems.append(f"{name} 被 index.html 引用，但 {dist}/ 裡沒有這個檔")
    elif open(path, "rb").read() not in data:
        problems.append(f"{name} 的內容不在 binary 裡：前端重建後 binary 沒有重編")
if problems:
    for p in problems:
        print(f"error: {p}", file=sys.stderr)
    print("error: 嵌入的前端與 web/dist 不一致：重編 agents-managerd（crates/am-base/build.rs 盯著 web/dist，正常的 cargo build --release 就會重嵌）", file=sys.stderr)
    sys.exit(1)
print(f"ok: 嵌入的前端與 {dist}/ 相同（index.html＋{len(refs)} 個 asset）")
PY
