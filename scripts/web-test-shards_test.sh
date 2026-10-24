#!/usr/bin/env bash
# web-test-shards.sh 的紅綠分支契約：用假 bun 取代真的 bun test，不跑真的 web 測試。
# 覆蓋 all-green（failed／retried 皆為空陣列，bash 3.2 + set -u 下曾經炸）、
# 第一次紅單獨重跑轉綠、重跑仍紅整輪 rc=1。
set -u
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/web-test-shards.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"

# 假 bun：只認 `test --shard=i/n`；每個 shard 的第幾次呼叫由 $MOCK_DIR/calls-<i> 計數。
cat > "$tmp/bin/bun" <<'MOCK'
#!/usr/bin/env bash
set -u
# 每次呼叫的 argv 逐一記一行（[] 包起來），斷言參數精確，不靠計數。
printf '[%s]\n' "$@" >> "$MOCK_DIR/argv.log"
shard=""
for a in "$@"; do
    case "$a" in --shard=*) shard="${a#--shard=}" ;; esac
done
i="${shard%%/*}"
calls="$MOCK_DIR/calls-$i"
attempt=$(( $(cat "$calls" 2>/dev/null || echo 0) + 1 ))
echo "$attempt" > "$calls"
rc=0
case "$MOCK_MODE" in
    allgreen) ;;
    initfail) [ "$i" = 1 ] && [ "$attempt" = 1 ] && rc=1 ;;
    persistent) [ "$i" = 1 ] && rc=1 ;;
    *) echo "unknown MOCK_MODE" >&2; exit 2 ;;
esac
echo "(mock) shard $i attempt $attempt"
if [ "$rc" -ne 0 ]; then
    echo "(fail) mock shard $i"
fi
echo "Ran 2 tests across 1 files."
exit "$rc"
MOCK
chmod +x "$tmp/bin/bun"

# run <mode> <name>：跑一次 web-test-shards.sh，輸出寫到 $tmp/<name>.{out,err}，rc 寫到 $tmp/<name>.rc。
run() {
    local mode="$1" name="$2"
    export MOCK_MODE="$mode" MOCK_DIR="$tmp/$name"
    mkdir -p "$MOCK_DIR"
    PATH="$tmp/bin:$PATH" CHECK_WEB_SHARDS=2 bash "$script" > "$tmp/$name.out" 2> "$tmp/$name.err"
    echo "$?" > "$tmp/$name.rc"
}

failures=0
expect() {
    # expect <label> <condition-result>；條件失敗就記一筆
    if [ "$2" != 0 ]; then
        echo "FAIL: $1" >&2
        failures=$((failures + 1))
    else
        echo "ok: $1"
    fi
}
calls_of() { cat "$tmp/$1/calls-$2" 2>/dev/null || echo 0; }
# argv_exact <name>：bun 收到的參數必須精確是 [test] 與 [--shard=i/2]，不得帶引號或反斜線。
argv_exact() {
    local log="$tmp/$1/argv.log"
    grep -qxF '[--shard=1/2]' "$log" && grep -qxF '[--shard=2/2]' "$log" \
        && [ -z "$(grep -vxE '\[test\]|\[--shard=[12]/2\]' "$log")" ]
}

# 1) all-green：failed 與 retried 都是空陣列，不得炸、不得重跑
run allgreen allgreen
expect "all-green rc=0" "$([ "$(cat "$tmp/allgreen.rc")" = 0 ] && echo 0 || echo 1)"
expect "all-green 不印重跑訊息" "$(grep -q '單獨重跑' "$tmp/allgreen.out" && echo 1 || echo 0)"
expect "all-green 不印 !! 警示" "$([ -s "$tmp/allgreen.err" ] && echo 1 || echo 0)"
expect "all-green 兩個 shard 各跑一次" "$([ "$(calls_of allgreen 1)" = 1 ] && [ "$(calls_of allgreen 2)" = 1 ] && echo 0 || echo 1)"
expect "all-green 彙總行 2 個 shard、4 條" "$(grep -q '==> bun test：2 個 shard，共 4 條、2 檔' "$tmp/allgreen.out" && echo 0 || echo 1)"
expect "all-green bun argv 精確 --shard=i/2" "$(argv_exact allgreen && echo 0 || echo 1)"

# 2) 第一次紅、單獨重跑轉綠：rc=0，點名警示
run initfail initfail
expect "initial-fail rc=0" "$([ "$(cat "$tmp/initfail.rc")" = 0 ] && echo 0 || echo 1)"
expect "initial-fail 印單獨重跑" "$(grep -q 'shard 1/2 第一次紅，單獨重跑' "$tmp/initfail.out" && echo 0 || echo 1)"
expect "initial-fail stderr 點名 shard 1" "$(grep -q '!! shard 1 平行跑時紅、單獨重跑綠' "$tmp/initfail.err" && echo 0 || echo 1)"
expect "initial-fail shard 1 跑兩次、shard 2 跑一次" "$([ "$(calls_of initfail 1)" = 2 ] && [ "$(calls_of initfail 2)" = 1 ] && echo 0 || echo 1)"
expect "initial-fail 重跑的 bun argv 仍精確 --shard=1/2" "$(grep -cxF '[--shard=1/2]' "$tmp/initfail/argv.log" | grep -qx 2 && argv_exact initfail && echo 0 || echo 1)"

# 3) 重跑仍紅：rc=1，點名仍紅的 shard
run persistent persistent
expect "persistent-fail rc=1" "$([ "$(cat "$tmp/persistent.rc")" = 1 ] && echo 0 || echo 1)"
expect "persistent-fail stderr 點名仍紅 shard" "$(grep -q '重跑仍紅的 shard：1' "$tmp/persistent.err" && echo 0 || echo 1)"
expect "persistent-fail shard 1 跑兩次" "$([ "$(calls_of persistent 1)" = 2 ] && echo 0 || echo 1)"
expect "persistent-fail bun argv 精確 --shard=i/2" "$(argv_exact persistent && echo 0 || echo 1)"

if [ "$failures" -ne 0 ]; then
    echo "web-test-shards_test: $failures 條失敗" >&2
    exit 1
fi
echo "web-test-shards_test: 全部通過"
