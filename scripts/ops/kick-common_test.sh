#!/bin/bash
# kick-common.sh 缺席時的行為：每支 source 它的 kick 都不能靜默停擺——
# 寫 log、不經 lib 直接 agm ops-alert、exit 1（agm 不在就只留 log，照樣 exit 1）。
#
#   bash scripts/ops/kick-common_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0; FAIL=0
ok() { echo "ok   - $1"; PASS=$((PASS + 1)); }
bad() { echo "FAIL - $1"; FAIL=$((FAIL + 1)); }

for k in ci-watch daemon-update herdr-update release-triage browser-gc; do
  for with_agm in 1 0; do
    R=$(mktemp -d); mkdir -p "$R/agm/bin"
    cp "$HERE/$k-kick.sh" "$R/agm/bin/$k-kick.sh"      # 刻意不放 kick-common.sh
    if [ "$with_agm" = 1 ]; then
      printf '#!/bin/bash\necho "$*" >> "%s/calls.log"\n' "$R" > "$R/agm/bin/agm"; chmod +x "$R/agm/bin/agm"
    fi
    AGM_DIR="$R/agm" bash "$R/agm/bin/$k-kick.sh" >/dev/null 2>&1; rc=$?
    tag="${k}（agm ${with_agm}）"
    [ "$rc" = 1 ] && ok "${tag}：exit 1" || bad "${tag}：exit ${rc}，預期 1"
    cat "$R"/agm/*.log 2>/dev/null | grep -q 'ALERT kick_common_missing' && ok "${tag}：有寫 log" || bad "${tag}：沒寫 log"
    if [ "$with_agm" = 1 ]; then
      grep -q -- '--reason kick_common_missing' "$R/calls.log" 2>/dev/null && ok "${tag}：有推 ops-alert" || bad "${tag}：沒推 ops-alert"
    else
      [ ! -e "$R/calls.log" ] && ok "${tag}：agm 不在只留 log" || bad "${tag}：不該有 agm 呼叫"
    fi
    rm -rf "$R"
  done
done
echo "kick-common_test: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
