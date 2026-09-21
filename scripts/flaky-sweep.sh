#!/bin/bash
# 一次抓出「整樹高負載下偶發紅」的一族：把 daemon 整樹測試在高並行下連跑 N 輪，可同時開多份製造負載，
# 最後彙總「任何一輪紅過的測試」與各自紅了幾輪。單跑都綠、整樹才紅的那種（#255／#274 一族）靠這支抓。
#
# 用法：scripts/flaky-sweep.sh [-n 輪數=5] [-c 同時幾份=2] [-t RUST_TEST_THREADS=64] [-o 輸出目錄] [-f 測試名過濾] [-k]
#   -k  跳過編譯，直接用上次的測試 binary（改了程式要重編才會反映）
# 輸出：<輸出目錄>/<份>-<輪>.log 是每一輪的完整輸出；最後列出清單（紅過的測試、紅了幾輪／總輪數、第一個 panic 訊息），
# 有任何一輪紅就 exit 1。用私有的 CARGO_TARGET_DIR（放在 target/flaky-sweep，已被 .gitignore），不碰共用的 target。
set -u
ROUNDS=5 COPIES=2 THREADS=64 OUT="" FILTER="" SKIP_BUILD=0
while getopts "n:c:t:o:f:k" o; do
  case $o in
    n) ROUNDS=$OPTARG ;; c) COPIES=$OPTARG ;; t) THREADS=$OPTARG ;; o) OUT=$OPTARG ;; f) FILTER=$OPTARG ;; k) SKIP_BUILD=1 ;;
    *) sed -n '2,10p' "$0"; exit 2 ;;
  esac
done
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${OUT:-$(mktemp -d "${TMPDIR:-/tmp}/flaky-sweep.XXXXXX")}
mkdir -p "$OUT"
export CARGO_TARGET_DIR=${FLAKY_TARGET_DIR:-$ROOT/target/flaky-sweep}
cd "$ROOT" || exit 2

if [ "$SKIP_BUILD" = 0 ]; then
  echo "編譯測試 binary…" >&2
  cargo test -p agents-managerd --no-run 2>"$OUT/build.log" || { tail -20 "$OUT/build.log"; exit 2; }
fi
BIN=$(cargo test -p agents-managerd --no-run --message-format=json 2>/dev/null \
  | sed -n 's/.*"executable":"\([^"]*agents_managerd-[^"]*\)".*/\1/p' | tail -1)
[ -x "$BIN" ] || { echo "找不到測試 binary（試試不加 -k）" >&2; exit 2; }
echo "binary: $BIN  輪數=$ROUNDS 同時=$COPIES threads=$THREADS  輸出: $OUT" >&2

run_copy() { # $1=份
  cd "$ROOT/daemon" || return
  for r in $(seq 1 "$ROUNDS"); do
    "$BIN" --test-threads="$THREADS" $FILTER >"$OUT/$1-$r.log" 2>&1
    echo "$1 $r $?" >>"$OUT/rounds.txt"
  done
}
: >"$OUT/rounds.txt"
for c in $(seq 1 "$COPIES"); do run_copy "$c" & done
wait

TOTAL=$(wc -l <"$OUT/rounds.txt" | tr -d ' ')
BAD=$(awk '$3!=0' "$OUT/rounds.txt" | wc -l | tr -d ' ')
echo
echo "共 $TOTAL 輪，紅了 $BAD 輪"
# 每個紅過的測試：紅了幾輪（同一輪只算一次）＋第一個 panic 訊息。
grep -h "^test .* \.\.\. FAILED$" "$OUT"/*-*.log 2>/dev/null | sed 's/^test \(.*\) \.\.\. FAILED$/\1/' | sort | uniq -c | sort -rn | while read -r n name; do
  first=$(grep -l "^test $name \.\.\. FAILED$" "$OUT"/*-*.log | head -1)
  msg=$(grep -h -A3 "^thread '$name'" "$first" 2>/dev/null | sed -n '2,4p' | tr '\n' ' ' | cut -c1-200)
  printf '%3d/%s  %s\n      %s\n' "$n" "$TOTAL" "$name" "$msg"
done
[ "$BAD" = 0 ]
