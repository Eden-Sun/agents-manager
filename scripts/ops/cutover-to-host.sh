#!/bin/bash
set -u -o pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
HELPER="$HERE/cutover-helper.py"
PT="${PROJECT_TRANSFER:-$HERE/project-transfer}"
TT="${TRANSCRIPT_TRANSFER:-$HERE/transcript-transfer}"
HOST_STATE_TOOL="${HOST_STATE_TRANSFER:-$HERE/host-state-transfer.py}"
PY="${PYTHON_BIN:-python3}"
SSH="${SSH_BIN:-ssh}"
RSYNC="${RSYNC_BIN:-rsync}"
LAUNCHCTL="${LAUNCHCTL_BIN:-launchctl}"
LSOF="${LSOF_BIN:-lsof}"

TARGET="${CUTOVER_TARGET:-ubuntu@agm-host}"
TARGET_NAME="${CUTOVER_TARGET_NAME:-agm-host}"
SRC_REPO="${CUTOVER_SRC_REPO:-/Users/me/project/agents-manager}"
DST_REPO="${CUTOVER_DST_REPO:-/home/user/project/agents-manager}"
SRC_DATA="${CUTOVER_SRC_DATA:-$HOME/.config/agents-manager}"
DST_DATA="${CUTOVER_DST_DATA:-/home/user/.config/agents-manager}"
SRC_API="${CUTOVER_SRC_API:-http://127.0.0.1:7788}"
DST_API="${CUTOVER_DST_API:-http://127.0.0.1:7788}"
PORT="${CUTOVER_PORT:-7788}"
IFS='|' read -r -a LABELS <<< "${CUTOVER_PROJECTS:-Agents Manager|AGM-DM-GRUP}"
SUP_LABEL="${CUTOVER_SUPERVISOR_PROJECT:-AGM-DM-GRUP}"
SUP_BOT="${CUTOVER_SUPERVISOR_BOT:-AGM}"
RESP_BOT="${CUTOVER_RESPONDER_BOT:-AGM-responder}"
NO_RESUME="${CUTOVER_NO_RESUME:-agm-pxf2pv-browser-gc}"
DAEMON_WAIT="${CUTOVER_DAEMON_WAIT_SECS:-60}"
STOP_WAIT="${CUTOVER_STOP_WAIT_SECS:-180}"
MAPS=("$SRC_REPO=$DST_REPO" "$SRC_DATA=$DST_DATA")
ERROR_COUNT="sed 's/\\x1b\\[[0-9;]*m//g' | grep -cE '^[0-9TZ:.-]+ +ERROR '"

CMD="${1:-}"
[ $# -gt 0 ] && shift
ORIG_ARGS=("$@")
EXECUTE=0; FOREGROUND=0; FROM=""; STATE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --execute) EXECUTE=1 ;;
        --foreground) FOREGROUND=1 ;;
        --from) FROM="${2:?--from 要接步驟名}"; shift ;;
        --state-dir) STATE="${2:?--state-dir 要接目錄}"; shift ;;
        -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
        *) echo "cutover-to-host: 不認得的參數 $1" >&2; exit 2 ;;
    esac
    shift
done

log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*"; }
warn() { log "WARN: $*"; WARNINGS=$((WARNINGS + 1)); }
die() { log "FATAL: $*" >&2; exit 1; }
WARNINGS=0

x() {
    if [ "$EXECUTE" = 1 ]; then
        log "RUN: $*" >&2
        "$@"
    else
        echo "    [dry-run] $*" >&2
    fi
}

q() { printf '%q ' "$@"; }
rsh() { "$SSH" -o BatchMode=yes "$TARGET" "$@"; }
rhelper() { "$SSH" -o BatchMode=yes "$TARGET" "python3 - $(q --base "$DST_API" --token-file "$DST_DATA/ui-token" "$@")" < "$HELPER"; }
shelper() { "$PY" "$HELPER" --base "$SRC_API" --token-file "$SRC_DATA/ui-token" "$@"; }
map_args() { local m; for m in "${MAPS[@]}"; do printf '%s\n' "$1" "$m"; done; }

TIMES=()
STEP_T0=0
DOWN_T0=""
step_begin() { log "== $1"; STEP_T0=$(date +%s); }
step_end() {
    local secs=$(( $(date +%s) - STEP_T0 ))
    TIMES+=("$1	$secs")
    [ -n "$STATE" ] && [ -d "$STATE" ] && printf '%s\t%s\t%s\n' "$CMD" "$1" "$secs" >> "$STATE/timings.tsv"
}
print_times() {
    local line total=0
    log "每步耗時（秒）："
    for line in ${TIMES[@]+"${TIMES[@]}"}; do printf '    %-12s %s\n' "${line%%	*}" "${line#*	}"; total=$((total + ${line#*	})); done
    printf '    %-12s %s\n' "合計" "$total"
}

STEPS_cutover="preflight freeze record stop-bots hand-off stop-src export transcripts ship stop-dst import start-dst resume verify timers"
STEPS_rollback="stop-dst restore start-dst start-src hand-back resume-src thaw"

should_run() {
    [ -z "$FROM" ] && return 0
    [ "$1" = preflight ] && return 0
    [ "${SKIPPING:-1}" = 0 ] && return 0
    if [ "$1" = "$FROM" ]; then SKIPPING=0; return 0; fi
    return 1
}

project_ids() { "$PY" -c 'import json,sys; [print(p["id"]) for p in json.load(open(sys.argv[1]))["projects"]]' "$1"; }
project_id_of() { "$PY" -c 'import json,sys; print(next(p["id"] for p in json.load(open(sys.argv[1]))["projects"] if p["label"]==sys.argv[2]))' "$1" "$2"; }

write_running() {
    "$PY" - "$1" "$2" "$SUP_LABEL" "$SUP_BOT" "$RESP_BOT" "$NO_RESUME" <<'EOF'
import json, os, sys
snap, out, sup_label, sup_bot, resp_bot, no_resume = sys.argv[1:7]
skip = set(no_resume.split())
active = ("starting", "running", "stopping")
projects = json.load(open(snap))["projects"]
res = {"resume": [], "names": [], "supervisor": False, "responder": False, "children": [], "not_resumed": []}
for p in projects:
    for b in p["bots"]:
        if b["run_state"] not in active:
            continue
        if b["managed_by"] == "child":
            res["children"].append(b["name"])
            continue
        if b["name"] in skip:
            res["not_resumed"].append(b["name"])
            continue
        res["resume"].append(b["id"]); res["names"].append(b["name"])
        if p["label"] == sup_label and b["name"] == sup_bot:
            res["supervisor"] = True
        if p["label"] == sup_label and b["name"] == resp_bot:
            res["responder"] = True
fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump(res, f, ensure_ascii=False, indent=2)
print(f"接回 {len(res['resume'])} 顆：{', '.join(res['names']) or '—'}；協調者={res['supervisor']} 巡檢={res['responder']}")
if res["children"]:
    print(f"在跑的 child {len(res['children'])} 顆（不會自動接回，由母 bot 重開）：{', '.join(res['children'])}")
if res["not_resumed"]:
    print(f"在跑但不在目標接回：{', '.join(res['not_resumed'])}")
EOF
}

running_field() { "$PY" -c 'import json,sys; v=json.load(open(sys.argv[1]))[sys.argv[2]]; print("\n".join(v) if isinstance(v,list) else ("1" if v else ""))' "$1" "$2"; }

src_listener() { "$LSOF" -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | head -1; }

wait_src_lock() {
    local i
    for i in $(seq "$DAEMON_WAIT"); do
        "$PY" "$HELPER" lock-free "$SRC_DATA" && return 0
        sleep 1
    done
    die "Mac 的 daemon.lock ${DAEMON_WAIT} 秒內沒放開"
}

stop_dst_daemon() {
    rsh "set -u; P=\$(ss -Hltnp 'sport = :$PORT' | sed -n 's/.*pid=\([0-9]*\).*/\1/p' | head -1)
if [ -z \"\$P\" ]; then echo 'target daemon not listening'; else
  C=\$(ps -o comm= -p \"\$P\"); case \"\$C\" in *agents-managerd*) kill \"\$P\"; echo \"killed \$P (\$C)\";; *) echo \"port $PORT held by \$C (pid \$P), not a daemon\" >&2; exit 1;; esac
fi" || return 1
    local i
    for i in $(seq "$DAEMON_WAIT"); do
        rhelper lock-free "$DST_DATA" && return 0
        sleep 1
    done
    log "目標 daemon.lock ${DAEMON_WAIT} 秒內沒放開"; return 1
}

start_dst_daemon() {
    rsh "set -u; export XDG_RUNTIME_DIR=\${XDG_RUNTIME_DIR:-/run/user/\$(id -u)}
N=\$(wc -l < $(q "$DST_DATA/daemon.log") 2>/dev/null || echo 0); echo \"\$N\" > $(q "$DST_DATA/.cutover-log-mark")
systemd-run --user --collect --unit=$(q "$1") -p Type=forking -p KillMode=process -- /usr/bin/python3 $(q "$DST_REPO/scripts/ops/daemon-start.py") $(q "$DST_REPO") $(q "$DST_DATA/daemon.log")
for i in \$(seq $DAEMON_WAIT); do curl -sf -o /dev/null http://127.0.0.1:$PORT/api/session && exit 0; sleep 1; done
echo 'target daemon did not answer /api/session' >&2; exit 1"
}

do_preflight() {
    command -v "$PY" >/dev/null || die "找不到 $PY"
    "$PY" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' \
        || die "來源 python3 要 3.11 以上（tomllib）；必要時設定 PYTHON_BIN"
    [ -f "$SRC_DATA/ui-token" ] || die "讀不到 $SRC_DATA/ui-token"
    [ -f "$SRC_DATA/config.toml" ] || die "讀不到 $SRC_DATA/config.toml"
    [ -f "$HOST_STATE_TOOL" ] || die "找不到 host state 工具：$HOST_STATE_TOOL"
    "$PY" "$HOST_STATE_TOOL" --help >/dev/null 2>&1 || die "host state 工具不能執行：$HOST_STATE_TOOL"
    local pre="$STATE/snapshot-preflight.json"
    local args=() l
    for l in "${LABELS[@]}"; do args+=(--label "$l"); done
    if [ -z "$FROM" ] || [ ! -f "$STATE/snapshot.json" ]; then
        shelper snapshot "${args[@]}" --out "$pre" | sed 's/^/    /' || die "Mac daemon 讀不到兩個專案（daemon 沒在跑？）"
        "$PY" -c 'import json,sys; b=[p["label"] for p in json.load(open(sys.argv[1]))["projects"] if p["handed_off_to"]]; sys.exit(1 if b else 0)' "$pre" \
            || die "有專案已經設了 handed_off_to：上一次切換沒做完？用 --from 接著做，或先 rollback"
        write_running "$pre" "$STATE/running-preflight.json" | sed 's/^/    /'
        [ -n "$(running_field "$STATE/running-preflight.json" children)" ] && warn "有 child 在跑：切換會停掉它們，工作沒收尾的等它們做完再切"
    fi
    "$PY" "$PT" export --help 2>/dev/null | grep -q -- --with-supervisor \
        || { [ "$EXECUTE" = 1 ] && die "$PT 沒有 --with-supervisor（#720 還沒進來），協調者資料會漏搬"; warn "$PT 沒有 --with-supervisor（#720 還沒進來）"; }
    [ -x "$TT" ] || [ -f "$TT" ] || die "找不到 ${TT}（#717）"
    rsh true || die "ssh $TARGET 不通"
    rsh "python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)'" || die "目標 python3 要 3.11 以上（tomllib）"
    rsh "test -f $(q "$DST_DATA/agents-manager.sqlite3") && test -f $(q "$DST_DATA/config.toml")" || die "目標沒有 $DST_DATA 的 DB／config（daemon 要先起過一次）"
    rsh "test -x $(q "$DST_REPO/target/release/agents-managerd") && test -f $(q "$DST_REPO/scripts/ops/daemon-start.py")" \
        || die "目標 $DST_REPO 沒有 release binary 或 daemon-start.py"
    rsh "command -v systemd-run >/dev/null" || die "目標沒有 systemd-run"
    rsh "systemctl --user cat herdr@.service >/dev/null 2>&1" || warn "目標沒裝 herdr@.service（#677）：daemon 會退回直接 spawn herdr，停 daemon 時 pane 會陪葬（SPEC §19）"
    local here_head dst_head
    here_head=$(git -C "$HERE" rev-parse HEAD 2>/dev/null)
    dst_head=$(rsh "git -C $(q "$DST_REPO") rev-parse HEAD" 2>/dev/null)
    if [ -n "$here_head" ] && ! rsh "git -C $(q "$DST_REPO") merge-base --is-ancestor $here_head HEAD" 2>/dev/null; then
        warn "目標 checkout（${dst_head:0:8}）不含這支腳本的版本（${here_head:0:8}）：先在目標 pull＋build release，否則 import 可能因欄位不足被拒"
    fi
    local snap="$pre" id
    [ -f "$snap" ] || snap="$STATE/snapshot.json"
    project_id_of "$snap" "$SUP_LABEL" >/dev/null 2>&1 \
        || die "快照裡找不到協調者專案（CUTOVER_SUPERVISOR_PROJECT=${SUP_LABEL}）：它要在 CUTOVER_PROJECTS 裡，否則協調者資料會漏搬"
    for id in $(project_ids "$snap"); do
        rsh "grep -q -F $(q "\"$id\"") $(q "$DST_DATA/config.toml")" && warn "目標 config.toml 已有專案 ${id}：之前 import 過（import 冪等、會跳過已有的列）"
    done
    local wt left=""
    while read -r wt; do
        [ -d "$wt" ] || continue
        if [ -n "$(git -C "$wt" status --porcelain --untracked-files=no 2>/dev/null | head -1)" ] \
            || [ -n "$(git -C "$wt" log --oneline -1 HEAD --not --remotes 2>/dev/null)" ]; then
            left="$left ${wt#"$SRC_REPO"/}"
        fi
    done < <(git -C "$SRC_REPO" worktree list --porcelain 2>/dev/null | sed -n 's/^worktree //p')
    [ -n "$left" ] && warn "這些 worktree 有沒推的 commit 或沒提交的改動，目標看不到（切換前推上去或放棄）：${left}"
    return 0
}

do_freeze() {
    local plist label
    : > "$STATE/launchd.txt"
    for plist in "$HOME"/Library/LaunchAgents/com.agm.*.plist; do
        [ -f "$plist" ] || continue
        label=$(basename "$plist" .plist)
        "$LAUNCHCTL" list "$label" >/dev/null 2>&1 || continue
        echo "$label" >> "$STATE/launchd.txt"
        x "$LAUNCHCTL" bootout "gui/$(id -u)/$label" || die "停用 launchd job 失敗：$label"
    done
    log "載入中的 com.agm.*：$(tr '\n' ' ' < "$STATE/launchd.txt")（dev.agents-manager.herdr-* 不動：已移交的 hub 專案還在 Mac 的 herdr 裡跑）"
}

do_record() {
    local args=() l
    for l in "${LABELS[@]}"; do args+=(--label "$l"); done
    shelper snapshot "${args[@]}" --out "$STATE/snapshot.json" | sed 's/^/    /' || die "快照失敗"
    write_running "$STATE/snapshot.json" "$STATE/running.json" | sed 's/^/    /' || die "在跑名單寫入失敗"
}

do_stop_bots() {
    DOWN_T0=$(date +%s); echo "$DOWN_T0" > "$STATE/down-since"
    x shelper api POST /api/supervisor/stop || die "協調者停不下來"
    x shelper api POST /api/supervisor/responder/stop || die "巡檢停不下來"
    local id
    for id in $(shelper active --snapshot "$STATE/snapshot.json"); do
        x shelper api POST "/api/bots/$id/stop" >/dev/null || warn "stop $id 失敗（下面等 run 結束時會再看）"
    done
    x shelper wait-idle --snapshot "$STATE/snapshot.json" --timeout "$STOP_WAIT" || die "bot 沒有全部停下來"
}

do_hand_off() {
    local id
    for id in $(project_ids "$STATE/snapshot.json"); do
        x shelper api PATCH "/api/projects/$id" --body "{\"handed_off_to\":\"$TARGET_NAME\"}" >/dev/null || die "設 handed_off_to 失敗：$id"
    done
}

do_stop_src() {
    local pid; pid=$(src_listener)
    if [ -z "$pid" ]; then log "Mac daemon 已經沒在聽 $PORT"; else
        case "$(ps -o comm= -p "$pid")" in
            *agents-managerd*) x kill "$pid" ;;
            *) die "Mac $PORT 的 listener（pid ${pid}）不是 agents-managerd" ;;
        esac
    fi
    [ "$EXECUTE" = 1 ] && wait_src_lock
    return 0
}

bundle_of() { echo "$STATE/$1.json.gz"; }
moved_of() { echo "$STATE/$1-moved.json.gz"; }

do_export() {
    local id sup_id margs=() m; sup_id=$(project_id_of "$STATE/snapshot.json" "$SUP_LABEL")
    for id in $(project_ids "$STATE/snapshot.json"); do
        if [ "$id" = "$sup_id" ]; then
            x "$PY" "$PT" export --db "$SRC_DATA/agents-manager.sqlite3" --project "$id" --out "$(bundle_of "$id")" --with-supervisor || die "export $id 失敗"
        else
            x "$PY" "$PT" export --db "$SRC_DATA/agents-manager.sqlite3" --project "$id" --out "$(bundle_of "$id")" || die "export $id 失敗"
        fi
    done
    for m in "${MAPS[@]}"; do margs+=(--map "$m"); done
    x "$PY" "$HOST_STATE_TOOL" snapshot --source-data "$SRC_DATA" --source-home "$HOME" \
        --out "$STATE/host-state" "${margs[@]}" || die "完整設定／token／outbox 快照失敗"
}

do_transcripts() {
    local id m margs=()
    for m in "${MAPS[@]}"; do margs+=(--map "$m"); done
    for id in $(project_ids "$STATE/snapshot.json"); do
        if [ "$EXECUTE" = 1 ]; then
            local rc=0 allowed=0
            "$PY" "$TT" --bundle "$(bundle_of "$id")" --out "$(moved_of "$id")" --target "$TARGET" "${margs[@]}" > "$STATE/transcripts-$id.json" || rc=$?
            case "$rc" in
                0) ;;
                2) allowed=$("$PY" "$HELPER" transcript-gate --bundle "$(bundle_of "$id")" --report "$STATE/transcripts-$id.json" --running "$STATE/running.json") \
                       || die "transcript-transfer ${id} 有擋下來的項目（見上），細節在 $STATE/transcripts-${id}.json"
                   warn "${id}：${allowed} 段舊 session 在 Mac 上已經沒有檔（不是要接回的最後一段），放行" ;;
                *) die "transcript-transfer ${id} 結束碼 ${rc}：看 $STATE/transcripts-${id}.json" ;;
            esac
            echo "$allowed" > "$STATE/transcripts-allowed-$id"
        else
            x "$PY" "$TT" --bundle "$(bundle_of "$id")" --out "$(moved_of "$id")" --target "$TARGET" "${margs[@]}"
        fi
    done
    x "$RSYNC" -a --exclude '*.lease-token.*' --exclude '*.lock' "$SRC_DATA/supervisor/" "$TARGET:$DST_DATA/supervisor/" \
        || die "supervisor 目錄傳輸失敗：$STATE 可重跑 transcripts"
}

RSTATE_REL="cutover"
rstate() { echo "$DST_DATA/$RSTATE_REL/$(basename "$STATE")"; }

do_ship() {
    local id files=()
    for id in $(project_ids "$STATE/snapshot.json"); do files+=("$(moved_of "$id")"); done
    if [ "$EXECUTE" = 1 ] && [ -f "$STATE/ship-transferred" ]; then
        log "所有 bundle 已傳完；接著清理來源副本"
    else
        x rsh "mkdir -p -m 700 $(q "$(rstate)")" || die "目標 cutover 目錄建立失敗"
        x "$RSYNC" -a "${files[@]}" "$HELPER" "$STATE/snapshot.json" "$STATE/running.json" "$TARGET:$(rstate)/" \
            || die "專案 bundle 傳輸失敗：$STATE 可重跑 ship"
        x "$RSYNC" -a "$PT" "$TARGET:$(rstate)/project-transfer" || die "project-transfer 工具傳輸失敗：$STATE 可重跑 ship"
        x "$RSYNC" -a "$STATE/host-state/" "$TARGET:$(rstate)/host-state/" || die "host-state 傳輸失敗：$STATE 可重跑 ship"
        x "$RSYNC" -a "$HOST_STATE_TOOL" "$TARGET:$(rstate)/host-state-transfer.py" || die "host-state 工具傳輸失敗：$STATE 可重跑 ship"
        x touch "$STATE/host-state-shipped" || die "host-state 傳輸標記寫入失敗"
        x touch "$STATE/ship-transferred" || die "傳輸完成標記寫入失敗"
    fi
    for id in $(project_ids "$STATE/snapshot.json"); do
        x rm -f "$(bundle_of "$id")" "$(moved_of "$id")" || die "已傳 bundle 的本機清理失敗：$STATE 可重跑 ship"
    done
    x rm -rf "$STATE/host-state" || die "已傳 host-state 的本機清理失敗：$STATE 可重跑 ship"
}

do_stop_dst() { x stop_dst_daemon || { log "停不了目標 daemon" >&2; return 1; }; }

do_import() {
    local id sup_id rs extra pmap=() m state_cmd cmd gone allowed idx
    local ids=() cmds=()
    sup_id=$(project_id_of "$STATE/snapshot.json" "$SUP_LABEL"); rs=$(rstate)
    for m in "${MAPS[@]}"; do pmap+=(--path-map "$m"); done
    for id in $(project_ids "$STATE/snapshot.json"); do
        extra=(); [ "$id" = "$sup_id" ] && extra=(--with-supervisor)
        cmd="python3 $(q "$rs/project-transfer") import --bundle $(q "$rs/$id-moved.json.gz") --host local $(q "${pmap[@]}") --config $(q "$DST_DATA/config.toml") ${extra[*]:-}"
        ids+=("$id"); cmds+=("$cmd")
        if [ "$EXECUTE" = 1 ]; then
            rsh "$cmd --dry-run" > "$STATE/import-dry-$id.json" || die "import --dry-run $id 失敗：看 $STATE/import-dry-$id.json"
            gone=$("$PY" -c 'import json, re, sys
for w in json.load(open(sys.argv[1])).get("warnings") or []:
    m = re.match(r"(\d+) 段原生對話的 transcript 不在這台", w)
    if m:
        print(m.group(1)); break' "$STATE/import-dry-$id.json") || die "讀不懂 import --dry-run 的摘要：$STATE/import-dry-${id}.json"
            allowed=$(cat "$STATE/transcripts-allowed-$id" 2>/dev/null || echo 0)
            [ -n "$gone" ] && [ "$gone" -gt "$allowed" ] \
                && die "import --dry-run ${id}：目標有 ${gone} 段 transcript 不在，對話搬移只放行 ${allowed} 段舊 session：$STATE/import-dry-${id}.json"
        else
            echo "    [dry-run] ssh $TARGET $cmd --dry-run   # 有 transcript 警告就停"
        fi
    done
    state_cmd="python3 $(q "$rs/host-state-transfer.py") install --bundle $(q "$rs/host-state") --target-data $(q "$DST_DATA") --backup-dir $(q "$rs/host-state-backup")"
    if [ "$EXECUTE" = 1 ]; then
        rsh "$state_cmd --dry-run" > "$STATE/host-state-dry-run.json" \
            || die "host-state install --dry-run 失敗：看 $STATE/host-state-dry-run.json"
        rsh "$state_cmd" > "$STATE/host-state-install.json" \
            || die "host-state install 失敗：看 $STATE/host-state-install.json"
    else
        echo "    [dry-run] ssh $TARGET $state_cmd --dry-run   # target daemon 必須已停，config 由 host-state 合併"
        echo "    [dry-run] ssh $TARGET $state_cmd"
    fi
    for idx in "${!ids[@]}"; do
        id=${ids[$idx]}; cmd=${cmds[$idx]}
        if [ "$EXECUTE" = 1 ]; then
            rsh "$cmd" > "$STATE/import-$id.json" || { log "import $id 失敗：看 $STATE/import-$id.json" >&2; return 1; }
            echo "$id" >> "$STATE/imports.txt"
        else
            echo "    [dry-run] ssh $TARGET $cmd"
        fi
    done
}

do_start_dst() { x start_dst_daemon "agents-managerd-$(basename "$STATE")" || { log "目標 daemon 起不來：看 $DST_DATA/daemon.log" >&2; return 1; }; }

do_resume() {
    local run="$STATE/running.json" id
    x rhelper api POST /api/supervisor/setup >/dev/null || warn "目標 supervisor setup 失敗"
    x rhelper api POST /api/supervisor/responder/setup >/dev/null || warn "目標 responder setup 失敗"
    for id in $(running_field "$run" resume); do
        x rhelper api POST "/api/bots/$id/start?resume=native" >/dev/null || warn "接回 $id 失敗"
    done
    local rc
    if [ -n "$(running_field "$run" supervisor)" ]; then
        x rhelper api POST /api/supervisor/start >/dev/null; rc=$?
        [ "$rc" = 0 ] || [ "$rc" = 3 ] || warn "目標 supervisor start 失敗（rc=${rc}）"
    fi
    if [ -n "$(running_field "$run" responder)" ]; then
        x rhelper api POST /api/supervisor/responder/start >/dev/null; rc=$?
        [ "$rc" = 0 ] || [ "$rc" = 3 ] || warn "目標 responder start 失敗（rc=${rc}）"
    fi
    if [ -f "$STATE/down-since" ]; then log "停機窗口：$(( $(date +%s) - $(cat "$STATE/down-since") )) 秒"; fi
}

do_verify() {
    local rs; rs=$(rstate)
    local m margs=()
    for m in "${MAPS[@]}"; do margs+=(--map "$m"); done
    if [ "$EXECUTE" != 1 ]; then echo "    [dry-run] 目標 helper verify＋daemon.log 新增 ERROR 行數＝0＋Mac $PORT 沒人聽"; return 0; fi
    local ok=0
    rsh "python3 $(q "$(rstate)/host-state-transfer.py") verify --bundle $(q "$(rstate)/host-state") --target-data $(q "$DST_DATA")" \
        | tee "$STATE/host-state-verify.json" || ok=1
    rhelper verify --snapshot "$rs/snapshot.json" --running "$rs/running.json" "${margs[@]}" | tee "$STATE/verify.json" || ok=1
    local errs; errs=$(rsh "N=\$(cat $(q "$DST_DATA/.cutover-log-mark") 2>/dev/null || echo 0); tail -n +\$((N + 1)) $(q "$DST_DATA/daemon.log") | $ERROR_COUNT" || true)
    [ "${errs:-0}" = 0 ] || { log "目標 daemon.log 起來後有 ${errs} 行 ERROR"; ok=1; }
    [ -z "$(src_listener)" ] || { log "Mac 的 $PORT 還有人在聽：兩邊同時開 daemon"; ok=1; }
    [ "$ok" = 0 ] || die "驗證沒過：修好或 rollback（$0 rollback --state-dir ${STATE}）"
    log "驗證通過。確認一陣子沒問題後刪目標 $(rstate)、$DST_DATA/*.pre-transfer-*"
}

do_timers() {
    x rsh "export XDG_RUNTIME_DIR=\${XDG_RUNTIME_DIR:-/run/user/\$(id -u)}; ls ~/.config/systemd/user/com.agm.*.timer >/dev/null 2>&1 || { echo '沒有 com.agm.*.timer：先 agm ops-sync 裝 #677 的 unit' >&2; exit 1; }; for t in ~/.config/systemd/user/com.agm.*.timer; do systemctl --user enable --now \"\$(basename \"\$t\")\"; done" \
        || warn "目標的例行 job 沒啟用（#677）"
}

do_restore() {
    local rs; rs=$(rstate)
    if [ -s "$STATE/imports.txt" ]; then
        local first; first=$(head -1 "$STATE/imports.txt")
        local db cfg
        db=$("$PY" -c 'import json,sys; print(next(b for b in json.load(open(sys.argv[1]))["backups"] if ".sqlite3.pre-transfer-" in b))' "$STATE/import-$first.json") || die "import-$first.json 裡找不到 DB 備份"
        cfg=$("$PY" -c 'import json,sys; print(next((b for b in json.load(open(sys.argv[1]))["backups"] if "config.toml.pre-transfer-" in b), ""))' "$STATE/import-$first.json")
        x rsh "set -e; cp $(q "$db") $(q "$DST_DATA/agents-manager.sqlite3"); rm -f $(q "$DST_DATA/agents-manager.sqlite3-wal") $(q "$DST_DATA/agents-manager.sqlite3-shm")" \
            || die "目標 DB 還原失敗"
        if [ -n "$cfg" ]; then
            x rsh "cp $(q "$cfg") $(q "$DST_DATA/config.toml")" || die "目標 config.toml 還原失敗"
        fi
    else
        log "沒有 project import 紀錄；只檢查是否要還原 host-state"
    fi
    if [ -f "$STATE/host-state-shipped" ]; then
        x rsh "if [ -f $(q "$rs/host-state-backup/backup.json") ]; then python3 $(q "$rs/host-state-transfer.py") restore --backup-dir $(q "$rs/host-state-backup") --target-data $(q "$DST_DATA"); else echo '沒有 host-state 備份'; fi" \
            || die "host-state rollback 失敗"
    else
        log "沒有送出 host-state bundle，不用還原 host-state"
    fi
    return 0
}

do_start_src() {
    [ -n "$(src_listener)" ] && { log "Mac daemon 已經在聽 $PORT"; return 0; }
    x "$LAUNCHCTL" submit -l "am-cutover-rollback-$$" -- "$PY" "$SRC_REPO/scripts/ops/daemon-start.py" "$SRC_REPO" "$SRC_DATA/daemon.log"
    [ "$EXECUTE" = 1 ] || return 0
    local i
    for i in $(seq "$DAEMON_WAIT"); do "${CURL_BIN:-curl}" -sf -o /dev/null "$SRC_API/api/session" && { "$LAUNCHCTL" remove "am-cutover-rollback-$$" 2>/dev/null; return 0; }; sleep 1; done
    die "Mac daemon 起不來：看 $SRC_DATA/daemon.log"
}

do_hand_back() {
    local id
    for id in $(project_ids "$STATE/snapshot.json"); do
        x shelper api PATCH "/api/projects/$id" --body '{"handed_off_to":null}' >/dev/null || die "清 handed_off_to 失敗：$id"
    done
}

do_resume_src() {
    local run="$STATE/running.json" id
    for id in $(running_field "$run" resume); do x shelper api POST "/api/bots/$id/start?resume=native" >/dev/null || warn "Mac 接回 $id 失敗"; done
    [ -n "$(running_field "$run" supervisor)" ] && { x shelper api POST /api/supervisor/start >/dev/null || true; }
    [ -n "$(running_field "$run" responder)" ] && { x shelper api POST /api/supervisor/responder/start >/dev/null || true; }
    return 0
}

do_thaw() {
    local label
    [ -f "$STATE/launchd.txt" ] || return 0
    while read -r label; do
        [ -n "$label" ] || continue
        x "$LAUNCHCTL" bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$label.plist" || die "launchd job 還原失敗：${label}"
    done < "$STATE/launchd.txt"
}

run_drill() {
    local drill="\$HOME/agm-$(basename "$STATE")" dhome
    DDIR=""
    DDIR=$(rsh "echo $drill") || die "ssh ${TARGET} 不通"; local ddir="$DDIR"
    dhome="$ddir/home"
    local wrap="$STATE/ssh-wrap"
    local margs=() pmap=() m id sup_id pt_sup=0
    for m in "${MAPS[@]}"; do margs+=(--map "$m"); pmap+=(--path-map "$m"); done
    cleanup_drill() {
        [ -n "$DDIR" ] && rsh "rm -rf $(q "$DDIR")" && log "已刪目標演練目錄 ${DDIR}（含 DB 複本）"
        rm -rf "$STATE" && log "已刪 Mac 狀態目錄 ${STATE}（含 bundle）"
    }
    trap cleanup_drill EXIT

    step_begin preflight
    local args=() l
    for l in "${LABELS[@]}"; do args+=(--label "$l"); done
    shelper snapshot "${args[@]}" --out "$STATE/snapshot.json" | sed 's/^/    /' || die "Mac daemon 讀不到兩個專案"
    project_id_of "$STATE/snapshot.json" "$SUP_LABEL" >/dev/null 2>&1 \
        || die "快照裡找不到協調者專案（CUTOVER_SUPERVISOR_PROJECT=${SUP_LABEL}）：它要在 CUTOVER_PROJECTS 裡，否則協調者資料會漏搬"
    "$PY" "$PT" export --help 2>/dev/null | grep -q -- --with-supervisor && pt_sup=1
    [ "$pt_sup" = 1 ] || warn "$PT 沒有 --with-supervisor（#720 未合）：演練不帶協調者資料"
    rsh "test -f $(q "$DST_DATA/agents-manager.sqlite3")" || die "目標沒有 DB"
    rsh "grep -q '^[[:space:]]*data_dir' $(q "$DST_DATA/config.toml")" && die "目標 config 有 [server] data_dir：複本會指回正式資料目錄，演練不做"
    step_end preflight

    step_begin export
    sup_id=$(project_id_of "$STATE/snapshot.json" "$SUP_LABEL")
    for id in $(project_ids "$STATE/snapshot.json"); do
        local sup=(); [ "$id" = "$sup_id" ] && [ "$pt_sup" = 1 ] && sup=(--with-supervisor)
        "$PY" "$PT" export --db "$SRC_DATA/agents-manager.sqlite3" --project "$id" --out "$(bundle_of "$id")" ${sup[@]+"${sup[@]}"} > "$STATE/export-$id.json" || die "export $id 失敗"
        log "  ${id}：$(ls -lh "$(bundle_of "$id")" | awk '{print $5}')，$("$PY" -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["counts"], "missing_files", len(d["missing_files"]))' "$STATE/export-$id.json")"
    done
    step_end export

    step_begin copy-db
    rsh "set -e; mkdir -p -m 700 $(q "$ddir/data") $(q "$dhome"); sqlite3 $(q "$DST_DATA/agents-manager.sqlite3") \".backup '$ddir/data/agents-manager.sqlite3'\"; cp $(q "$DST_DATA/config.toml") $(q "$ddir/data/config.toml"); chmod 600 $(q "$ddir/data/agents-manager.sqlite3")" || die "目標複本失敗"
    step_end copy-db

    step_begin transcripts
    mkdir -p "$wrap"
    local real_ssh; real_ssh=$(command -v "$SSH") || die "找不到 ${SSH}"
    cat > "$wrap/ssh" <<EOF
#!/bin/bash
args=("\$@"); n=\${#args[@]}; last=\${args[\$((n - 1))]}; unset "args[\$((n - 1))]"
exec $(q "$real_ssh") "\${args[@]}" "env HOME=$(q "$dhome") \$last"
EOF
    chmod +x "$wrap/ssh"
    [ "$("$wrap/ssh" -o BatchMode=yes "$TARGET" "python3 -c 'import os; print(os.path.expanduser(\"~\"))'")" = "$dhome" ] || die "ssh 包裝器沒把目標 HOME 換成 ${dhome}，不搬（會寫進正式的 ~/.claude）"
    for id in $(project_ids "$STATE/snapshot.json"); do
        PATH="$wrap:$PATH" "$PY" "$TT" --bundle "$(bundle_of "$id")" --out "$(moved_of "$id")" --target "$TARGET" "${margs[@]}" > "$STATE/transcripts-$id.json"
        local rc=$?
        log "  ${id}：rc=$rc $("$PY" -c 'import json,sys; d=json.load(open(sys.argv[1])); print("sessions", len(d["sessions"]), "written", d["files_written"], "missing", len(d["missing"]), "refused", len(d["refused"]), "conflicts", len(d["conflicts"]))' "$STATE/transcripts-$id.json" 2>/dev/null)"
        [ "$rc" = 0 ] || [ "$rc" = 2 ] || die "transcript-transfer $id 失敗（rc=${rc}）"
        [ "$rc" = 2 ] && warn "transcript-transfer $id 有 missing／refused／conflicts（見上）"
    done
    step_end transcripts

    step_begin ship
    "$RSYNC" -a "$HELPER" "$TARGET:$ddir/" && "$RSYNC" -a "$PT" "$TARGET:$ddir/project-transfer" || die "傳工具失敗"
    for id in $(project_ids "$STATE/snapshot.json"); do "$RSYNC" -a "$(moved_of "$id")" "$TARGET:$ddir/" || die "傳 bundle 失敗"; done
    step_end ship

    step_begin import
    for id in $(project_ids "$STATE/snapshot.json"); do
        local sup=""; [ "$id" = "$sup_id" ] && [ "$pt_sup" = 1 ] && sup="--with-supervisor"
        local cmd="python3 $(q "$ddir/project-transfer") import --bundle $(q "$ddir/$id-moved.json.gz") --host local $(q "${pmap[@]}") --config $(q "$ddir/data/config.toml") $sup"
        rsh "$cmd --dry-run" > "$STATE/import-dry-$id.json" || die "import --dry-run $id 失敗：$(tail -3 "$STATE/import-dry-$id.json")"
        rsh "$cmd" > "$STATE/import-$id.json" || die "import $id 失敗：$(tail -3 "$STATE/import-$id.json")"
        log "  ${id}：$("$PY" -c 'import json,sys; d=json.load(open(sys.argv[1])); print("inserted", d["inserted"]); d.get("supervisor") and print("    supervisor:", json.dumps(d["supervisor"], ensure_ascii=False)[:600]); [print("    warn:", w) for w in d["warnings"] if "用身份" not in w]' "$STATE/import-$id.json")"
    done
    step_end import

    step_begin verify
    local bad=0
    log "  搬到目標的對話：$(rsh "du -sh $(q "$dhome") | cut -f1; find $(q "$dhome") -type f | wc -l" | tr '\n' ' ')（大小、檔數）"
    for id in $(project_ids "$STATE/snapshot.json"); do
        rhelper drill-verify --db "$ddir/data/agents-manager.sqlite3" --config "$ddir/data/config.toml" --bundle "$ddir/$id-moved.json.gz" "${margs[@]}" \
            | sed 's/^/    /' || bad=1
    done
    step_end verify

    print_times
    log "演練結束（WARN ${WARNINGS} 則）"
    [ "$bad" = 0 ] || die "演練驗證沒過（見上）"
}

case "$CMD" in
    cutover|rollback|drill) ;;
    *) sed -n '2,12p' "$0"; exit 2 ;;
esac

if [ "$CMD" = drill ]; then
    EXECUTE=0
    STATE="$SRC_DATA/cutover/drill-$(date +%Y%m%d-%H%M%S)"
    mkdir -p "$SRC_DATA/cutover" && mkdir -m 700 "$STATE" || die "建不了 $STATE"
    run_drill
    exit 0
fi

if [ "$CMD" = rollback ] && [ -z "$STATE" ]; then die "rollback 要 --state-dir（cutover 的狀態目錄）"; fi
if [ -n "$FROM" ] && [ -z "$STATE" ]; then die "--from 要搭 --state-dir"; fi
eval "STEPS=\$STEPS_$CMD"
if [ -n "$FROM" ] && ! printf '%s\n' $STEPS | grep -qx -- "$FROM"; then die "沒有步驟 ${FROM}（${STEPS}）"; fi

TMP_STATE=""
if [ -z "$STATE" ]; then
    if [ "$EXECUTE" = 1 ]; then
        STATE="$SRC_DATA/cutover/$(date +%Y%m%d-%H%M%S)"
        mkdir -p "$SRC_DATA/cutover" && mkdir -m 700 "$STATE" || die "建不了 $STATE"
    else
        STATE=$(mktemp -d); TMP_STATE="$STATE"
        trap 'rm -rf "$TMP_STATE"' EXIT
    fi
fi
[ -d "$STATE" ] || die "沒有狀態目錄 $STATE"

if [ "$EXECUTE" = 1 ] && [ "$FOREGROUND" = 0 ] && [ -z "${CUTOVER_DETACHED:-}" ]; then
    export CUTOVER_DETACHED=1
    has_state=0; for a in ${ORIG_ARGS[@]+"${ORIG_ARGS[@]}"}; do [ "$a" = --state-dir ] && has_state=1; done
    extra=(); [ "$has_state" = 0 ] && extra=(--state-dir "$STATE")
    nohup "$PY" -c 'import os, sys; os.setsid(); os.execvp(sys.argv[1], sys.argv[1:])' \
        bash "$0" "$CMD" ${ORIG_ARGS[@]+"${ORIG_ARGS[@]}"} ${extra[@]+"${extra[@]}"} >> "$STATE/run.log" 2>&1 < /dev/null &
    echo "背景執行（pid $!），log：$STATE/run.log"
    echo "狀態目錄：$STATE"
    exit 0
fi

[ "$EXECUTE" = 1 ] || log "dry-run：只做唯讀檢查、列出會做的事。真的做加 --execute。"
for s in $STEPS; do
    should_run "$s" || { log "== ${s}（--from ${FROM}，跳過）"; continue; }
    step_begin "$s"
    "do_${s//-/_}" || die "步驟 ${s} 失敗；修好後用 --from ${s} --state-dir ${STATE} 接著做"
    step_end "$s"
done
print_times
log "完成（${CMD}，WARN ${WARNINGS} 則）。狀態目錄：$STATE"
