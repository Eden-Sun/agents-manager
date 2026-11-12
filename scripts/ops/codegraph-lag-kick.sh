#!/bin/bash
# codegraph 索引落後告警（週期）：主 checkout 的索引落後 HEAD 超過門檻就喊 ops-alert（reason=codegraph_index_lag）。
#
# 根因：主 checkout 曾停在舊分支，索引停在 11/10 好幾天，沒人發現；codegraph 的 watcher 只在 daemon 裡跑，壞了不會叫。
# 由 systemd user timer `com.agm.codegraph-lag.timer` 每 600 秒跑一次（Linux），純機械不花 LLM。
# 判斷與門檻見 codegraph-lag.py（落後 commit 數 > CG_LAG_MAX_COMMITS 或最舊未反映 commit 超過 CG_LAG_MAX_MINUTES 分鐘）。
# 只讀：不跑 sync、不改索引。要補索引請人手跑 `codegraph sync`（在主樹）。
#
# 喊人走 kick-common.sh 的 alert()（同 source+reason daemon 每小時去重）。索引缺席回 exit 2 也喊。
#
#   AGM_REPO、AGM_BIN、CG_LAG_PY、CG_LAG_LOG、CG_LAG_MAX_COMMITS、CG_LAG_MAX_MINUTES 可覆寫（測試用）。
set -u

DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOG="${CG_LAG_LOG:-$DIR/codegraph-lag.log}"
AGM="${AGM_BIN:-$DIR/bin/agm}"
OWNER=codegraph-lag
REPO="${AGM_REPO:-$HOME/project/agents-manager}"
PY="${CG_LAG_PY:-$DIR/bin/codegraph-lag.py}"
MAX_COMMITS="${CG_LAG_MAX_COMMITS:-20}"
MAX_MINUTES="${CG_LAG_MAX_MINUTES:-30}"

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }
# 共用函式庫（缺席時的處理同其他 kick：寫 log、直接 ops-alert、exit 1）。
. "${0%/*}/kick-common.sh" 2>/dev/null || {
  _m="找不到 ${0%/*}/kick-common.sh，${0##*/} 停擺（它要和 kick 一起裝進 bin/）"
  echo "$(date '+%F %T') ALERT kick_common_missing：$_m" >> "$LOG"; echo "$_m" >&2
  [ -x "$AGM" ] && "$AGM" --compact ops-alert --source "$OWNER" --reason kick_common_missing --detail "$_m" >> "$LOG" 2>&1
  exit 1
}

[ -r "$PY" ] || { alert codegraph_lag_check_missing "找不到 ${PY}，codegraph 索引落後檢查停擺"; exit 1; }
command -v python3 >/dev/null 2>&1 || { alert missing_dependency "找不到 python3，codegraph 索引落後檢查停擺"; exit 1; }

OUT=$(python3 -B "$PY" --repo "$REPO" --max-commits "$MAX_COMMITS" --max-minutes "$MAX_MINUTES" --json 2>&1)
RC=$?
log "rc=${RC} ${OUT}"

case "$RC" in
  0) exit 0 ;;
  1) alert codegraph_index_lag "主 checkout 的 codegraph 索引落後：${OUT}（門檻 ${MAX_COMMITS} commit／${MAX_MINUTES} 分鐘）。補救：在 ${REPO} 跑 codegraph sync" ;;
  *) alert codegraph_index_unreadable "讀不到 codegraph 索引（rc=${RC}）：${OUT}" ;;
esac
exit 0
