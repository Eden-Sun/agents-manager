#!/usr/bin/env python3
"""#267：「這顆 bot 怎麼了、下一步該建議什麼」的題目。

用法：build_cases.py ~/.config/agents-manager/daemon.log > cases.jsonl

樣本來源兩種，各自都在 `state.source` 標明：
  daemon_log  —— 本機 daemon.log 的維運訊息。每題是同一顆 bot／主機在事發前 20 分鐘的
                 視窗，最後一行就是 `trigger`；視窗只含事發之前的行，沒有事後的線索。
  supervisor  —— `GET /api/supervisor/inbox?all=1` 與 `?/incidents?all=1` 的紀錄
                 （2026-09-24 取），因為 inbox 只留最近 500 筆，內容遮罩後直接寫在下面。

標籤是我（Opus 5，i267）在呼叫 Jev 之前，看完「事發後 60 分鐘」的 log 決定的：
Jev 只看得到事發之前，標籤看得到之後，這是刻意的。判準寫在 REPORT.md「標註規則」。

遮罩：`hook received` 那類帶對話內容的行整條不取；email／UUID／ULID／sha／IP／家目錄
都換成佔位字串；bot 名字換成 bot-1。輸出會自我檢查，帶 token 樣態的字串直接中止。
"""
import json
import re
import sys

WINDOW_S = 20 * 60
MAX_LINES = 18

# 這些訊息不是維運事實（帶對話內容、或純粹是每秒心跳），視窗一律不收。
DROP = [
    "hook received", "reconcile: kept active run", "statusline received", "pane.agent_detected",
    "git remote lookup failed", "no identities known for this kind yet", "workspace closed",
    "slow statement:", "acquired connection, but time to acquire", "scanned non-agent panes",
    "hook ignored", "hook inbox drained", "kind preflight ok", "tools detected",
    "supervisor notify throttled", "watching pane agent status", "pane.agent_status_changed",
]

LINE = re.compile(r"^(\d{4}-\d\d-\d\dT[\d:.]+Z)\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+(.*)$")
KV = re.compile(r'(\w+)=(?:"([^"]*)"|(\S+))')
ANSI = re.compile(r"\033\[[0-9;]*m")
ULID = re.compile(r"\b[0-9A-HJKMNP-TV-Z]{26}\b")
UUID = re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
SHA = re.compile(r"\b[0-9a-f]{40}\b")
EMAIL = re.compile(r"\b[\w.+-]+@[\w-]+\.[\w.-]+\b")
IPV4 = re.compile(r"\b\d{1,3}(\.\d{1,3}){3}\b")
SECRET = re.compile(r"(?i)(bearer\s|sk-[a-z0-9]|api[-_]?key|x-am-token|password|secret)")

# (id, 時間戳前綴, 訊息片段, agent_kind, 標籤)
# 時間戳前綴＋片段命中多行時取最後一筆（同一秒連發的情形）；一行都沒命中就中止。
LOG_CASES = [
    # 額度：等重置就好，人不必做事
    ("q-fable-1", "2026-09-11T05:40:12", "turn cut short by an API error", "claude", "wait_quota"),
    ("q-fable-2", "2026-09-11T05:40:23", "turn cut short by an API error", "claude", "wait_quota"),
    ("q-fable-3", "2026-09-11T05:40:37", "turn cut short by an API error", "claude", "wait_quota"),
    ("q-park-1", "2026-09-14T02:40:28", "assignment 等額度回來", "claude", "wait_quota"),
    ("q-park-2", "2026-09-16T04:32:20", "assignment 等額度回來", "claude", "wait_quota"),
    ("q-stopfail-rate", "2026-09-21T11:47:48", "StopFailure", "claude", "wait_quota"),
    ("q-codex-banner", "2026-09-09T11:40:51", "codex account notice captured", "codex", "wait_quota"),
    # 不必動：暫時的、自己會好的、或根本是誤判
    ("na-api-500", "2026-09-22T00:57:46", "turn cut short by an API error", "claude", "no_action"),
    ("na-api-529", "2026-09-22T01:04:38", "turn cut short by an API error", "claude", "no_action"),
    ("na-codex-history", "2026-09-15T00:21:58", "codex limit banner on screen is history", "codex", "no_action"),
    ("na-codex-diff", "2026-09-10T13:18:35", "codex account notice captured", "codex", "no_action"),
    ("na-codex-source", "2026-09-10T13:04:46", "codex account notice captured", "codex", "no_action"),
    ("na-codex-reset", "2026-09-08T05:29:26", "codex account notice captured", "codex", "no_action"),
    ("na-subdrop", "2026-09-22T01:11:34", "pane status subscription dropped", "unknown", "no_action"),
    ("na-maint-gone", "2026-09-16T18:10:45", "reconcile: agent gone, marking run exited", "claude", "no_action"),
    ("na-hostconn", "2026-09-19T12:47:15", "host connect failed", "unknown", "no_action"),
    ("na-sleep", "2026-09-18T07:40:04", "agent did not exit within 10s", "claude", "no_action"),
    ("na-update-restart", "2026-09-12T01:43:47", "agent did not exit within 10s", "claude", "no_action"),
    ("na-stall-busy", "2026-09-22T01:12:33", "prompt stalled; turn failed", "claude", "no_action"),
    ("na-grok-park", "2026-09-16T20:59:36", "grok quota refresh failed; parking this host", "grok", "no_action"),
    ("na-noplan-loggedin", "2026-09-23T13:20:31", "claude reported no plan lines; parking this account", "claude", "no_action"),
    # 要人看：daemon 自己解不開，而且不是額度也不是登入
    ("es-ops-cd", "2026-09-20T21:05:05", "a scheduled ops script reported that it is stuck", "unknown", "escalate_user"),
    ("es-undeliv-1", "2026-09-16T11:18:11", "assignment could not be delivered; marked blocked", "claude", "escalate_user"),
    ("es-undeliv-2", "2026-09-16T10:19:18", "assignment could not be delivered; marked blocked", "claude", "escalate_user"),
    ("es-queue-too-long", "2026-09-19T03:49:01", "排隊太久", "claude", "escalate_user"),
    ("es-stall-screen", "2026-09-16T14:20:06", "prompt stalled; turn failed", "claude", "escalate_user"),
    ("es-paste-head", "2026-09-23T03:54:21", "the composer lost the head of the paste", "claude", "escalate_user"),
    ("es-qback-paste", "2026-09-23T06:13:32", "queued prompt put back on the queue", "claude", "escalate_user"),
    ("es-qback-transcript", "2026-09-22T15:35:11", "queued prompt put back on the queue", "claude", "escalate_user"),
    ("es-herdr-protocol", "2026-09-14T11:57:11", "could not read the pane before typing", "claude", "escalate_user"),
    # 重啟：agent 或它的 pane 已經卡死／不在了
    ("rs-updfail-1", "2026-09-09T08:06:49", "restart for the claude update failed", "claude", "restart_bot"),
    ("rs-updfail-2", "2026-09-09T11:36:40", "restart for the claude update failed", "claude", "restart_bot"),
    ("rs-wait-settle", "2026-09-14T23:21:52", "agent.wait did not settle", "claude", "restart_bot"),
    ("rs-stuck-pane", "2026-09-22T02:37:38", "closed a turn stuck in flight", "claude", "restart_bot"),
    ("rs-enter-loop", "2026-09-23T16:35:55", "Enter did not reach the pane", "claude", "restart_bot"),
    # 登入：要人把 CLI 登進去／指到對的帳號
    ("lg-cc2-first", "2026-09-07T16:45:17", "claude reported no plan lines; parking this identity", "claude", "ask_login"),
    ("lg-cc2-persisting", "2026-09-07T16:58:42", "claude reported no plan lines; parking this identity", "claude", "ask_login"),
    ("lg-remote-default", "2026-09-19T12:47:27", "claude reported no plan lines; parking this account", "claude", "ask_login"),
    ("lg-remote-named", "2026-09-19T12:47:36", "claude reported no plan lines; parking this account", "claude", "ask_login"),
    ("lg-wrong-identity", "2026-09-16T14:02:39", "identity not logged in on host", "claude", "ask_login"),
]

# supervisor inbox／incidents。`record` 是遮罩後的原文欄位，`trigger` 是那筆的種類。
SUP_CASES = [
    ("sup-restart-taken-1", "unknown", "restart_bot", "inbox bot_restart_failed", {
        "kind": "bot_restart_failed", "created_at": "2026-09-22T22:35:27Z",
        "payload": {"batch_id": "<id1>", "bot_id": "<id2>", "name": "rh",
                    "error": "herdr error agent_name_taken: agent name bot-1 is already used; "
                             "candidates: terminal_id=<id3> pane_id=w168:pD3 workspace_id=w168 "
                             "tab_id=w168:t6A cwd=~/project/agents-manager status=Done"}}),
    ("sup-restart-taken-2", "unknown", "restart_bot", "inbox bot_restart_failed", {
        "kind": "bot_restart_failed", "created_at": "2026-09-22T22:35:26Z",
        "payload": {"batch_id": "<id1>", "bot_id": "<id2>", "name": "pvd",
                    "error": "herdr error agent_name_taken: agent name bot-1 is already used; "
                             "candidates: terminal_id=<id3> pane_id=w168:pD2 workspace_id=w168 "
                             "tab_id=w168:t6A cwd=~/project/agents-manager status=Done"}}),
    ("sup-intent-expired", "unknown", "escalate_user", "inbox intent_failed", {
        "kind": "intent_failed", "created_at": "2026-09-22T05:11:14Z",
        "payload": {"attempts": 0, "host": "local", "intent_id": "<id1>", "kind": "restart",
                    "last_error": "expired before it could be completed", "subject_id": "<id2>",
                    "message": "restart 沒能補完（已試 0 次）：expired before it could be completed。請人工確認 <id2> 的狀態。"}}),
    ("sup-undeliv-paste", "claude", "escalate_user", "inbox assignment_undeliverable", {
        "kind": "assignment_undeliverable", "created_at": "2026-09-23T04:23:56Z",
        "payload": {"assignment_id": "<id1>", "attempts": 7, "conflict_since": "2026-09-23T03:52:06Z",
                    "reason": "paste_truncated", "status": "blocked", "target_bot_id": "<id2>",
                    "waited_mins": 30,
                    "hint": "這筆停在 blocked，不會再自己重試。要重派用 `bin/agm review <assignment_id> "
                            "--decision followup --followup-request-id <新的 id> --followup-text …`"
                            "（可加 `--followup-bot` 改派給別顆）；不要了就 `--decision cancel`。"}}),
    ("sup-undeliv-queue", "claude", "escalate_user", "inbox assignment_undeliverable", {
        "kind": "assignment_undeliverable", "created_at": "2026-09-22T20:23:50Z",
        "payload": {"assignment_id": "<id1>", "bot_id": "<id2>", "needs_review": True,
                    "reason": "排進佇列等了 30 分鐘，對方一直沒有回合結束的空檔，沒有送出",
                    "revoked_turn_id": "<id3>", "status": "blocked", "waited_s": 1805,
                    "hint": "排著的那則已撤回，不會再送。等那顆 bot 空下來再派一次（followup），或改派給別人。"}}),
    ("sup-ops-herdr-resume", "unknown", "escalate_user", "inbox ops_alert", {
        "kind": "ops_alert", "created_at": "2026-09-22T03:04:05Z",
        "payload": {"source": "daemon-update-kick", "reason": "check_failing",
                    "action": "這支排程腳本已經停住，自己解不開：照 detail 處理，處理完 ack",
                    "detail": "herdr 全重啟後，約 28 顆 agent 是被 herdr 自己的 resume_agents_on_restore "
                              "用不帶 daemon 參數的命令接回的（沒有 daemon 的 argv 與 AM_* 環境）。"
                              "~/.config/herdr/config.toml 第 185 行 resume_agents_on_restore = true，"
                              "和 SPEC §6.5.2 第 5 點（必須關掉）矛盾。影響：daemon.log 自 01:11Z 起沒有收到任何 hook"
                              "（前一小時 124 筆），回合結束只能靠 5 分鐘 idle fallback；這些 bot 沒有 bot token。"}}),
    ("sup-pane-unowned", "claude", "no_action", "inbox pane_unowned", {
        "kind": "pane_unowned", "created_at": "2026-09-23T16:13:20Z",
        "payload": {"foreground": "~/.local/bin/claude auth status --json", "host": "zz92",
                    "kind": "service", "owner_bot_id": None, "pane_id": "wP:p1", "project_id": None,
                    "message": "這顆 pane 對不到任何專案，而且不是那顆固定的 scratch"}}),
    ("sup-inc-stalled", "claude", "no_action", "incident assignment_stalled", {
        "kind": "assignment_stalled", "severity": "degraded", "first_seen_at": "2026-09-18T02:59:48Z",
        "occurrences": 1,
        "detail": {"assignment_id": "<id1>", "bot_id": "<id2>", "status": "awaiting_review",
                   "updated_at": "2026-09-18T00:59:21Z"}}),
    ("sup-inc-undelivered", "claude", "restart_bot", "incident assignment_undelivered", {
        "kind": "assignment_undelivered", "severity": "degraded", "first_seen_at": "2026-09-15T11:06:04Z",
        "occurrences": 20,
        "detail": {"assignment_id": "<id1>", "attempts": 30, "bot_id": "<id2>",
                   "created_at": "2026-09-15T09:05:41Z", "last_error": "bot has no active run"}}),
]

ACTIONS = {
    "wait_quota": {
        "what": "The account's usage or rate limit is what is stopping it; the right move is to wait for the limit to reset, or to park the work until it does",
        "not_for": "text that merely mentions a limit while the account still has headroom"},
    "ask_login": {
        "what": "A person has to sign the CLI in, or point it at the right account, before this agent can work"},
    "restart_bot": {
        "what": "The agent process or its pane is wedged, gone, or failed to come back, and starting it again is the right next step"},
    "escalate_user": {
        "what": "A person has to look at this and decide what to do: it is not a limit, not a login, and starting the agent again would not fix it"},
    "no_action": {
        "what": "Recommend nothing: this is transient, expected, already being handled automatically, or a false alarm"},
}

QUESTIONS = {
    "next_action": {
        "type": "choice",
        "instructions": "`evidence` ends at the moment a manager program noticed something wrong with one agent it drives. "
                        "`facts` are counts the manager computed from its own records, all from before that moment. "
                        "What should the manager recommend as the next step?",
        "criteria": ACTIONS},
    "quota_problem": {
        "type": "noul",
        "instructions": "Is the account's usage or rate limit the main thing stopping this agent right now?"},
    "auth_problem": {
        "type": "noul",
        "instructions": "Is a login, account or credential problem the main thing stopping this agent right now?"},
    "needs_human": {
        "type": "noul",
        "instructions": "Does a person have to do something before this agent can make progress again?"},
    "self_clearing": {
        "type": "noul",
        "instructions": "Will this clear by itself within the hour, with nobody doing anything about it?"},
    "restart_helps": {
        "type": "noul",
        "instructions": "Is starting this agent again the right next step?"},
}

DERIVED = {
    "quota_problem": lambda l: l == "wait_quota",
    "auth_problem": lambda l: l == "ask_login",
    "needs_human": lambda l: l in ("ask_login", "escalate_user"),
    "self_clearing": lambda l: l == "no_action",
    "restart_helps": lambda l: l == "restart_bot",
}


def mask(text, ids, names):
    text = ANSI.sub("", text)
    for n, repl in names.items():
        text = text.replace(n, repl)
    text = EMAIL.sub("<email>", text)
    text = SHA.sub("<sha>", text)
    text = UUID.sub("<uuid>", text)
    text = IPV4.sub("<ip>", text)
    text = text.replace("/Users/m4p", "~").replace("/home/ubuntu", "~")

    def one(m):
        return ids.setdefault(m.group(0), "<id%d>" % (len(ids) + 1))
    return ULID.sub(one, text)


def parse(path):
    rows = []
    for raw in open(path, errors="replace"):
        m = LINE.match(ANSI.sub("", raw.rstrip("\n")))
        if not m:
            continue
        msg = m.group(3)
        if any(d in msg for d in DROP):
            continue
        kv = {}
        for k, q, u in KV.findall(msg):
            kv.setdefault(k, q if q else u)
        rows.append({"iso": m.group(1), "lvl": m.group(2), "msg": msg[:400].rstrip(), "kv": kv})
    run2bot, turn2bot = {}, {}
    for r in rows:
        b = r["kv"].get("bot")
        if b:
            run2bot.setdefault(r["kv"].get("run", ""), b)
            turn2bot.setdefault(r["kv"].get("turn", ""), b)
    for r in rows:
        kv = r["kv"]
        b = kv.get("bot") or run2bot.get(kv.get("run", "")) or turn2bot.get(kv.get("turn", ""))
        r["bot"] = b
        r["key"] = ("bot:" + b) if b else next(
            (f + ":" + kv[f] for f in ("identity", "account", "assignment", "host") if kv.get(f)), None)
    return rows


def secs(iso):
    h, m, s = int(iso[11:13]), int(iso[14:16]), int(iso[17:19])
    d = (int(iso[0:4]) * 372 + int(iso[5:7]) * 31 + int(iso[8:10]))
    return d * 86400 + h * 3600 + m * 60 + s


def head(msg):
    """訊息前面那段不含 key=value 的字，用來數「同一種訊息」。"""
    out = []
    for w in msg.split():
        if "=" in w:
            break
        out.append(w)
    return " ".join(out)


def build_log_case(rows, cid, stamp, needle, kind, label):
    hits = [i for i, r in enumerate(rows) if r["iso"].startswith(stamp) and needle in r["msg"]]
    if not hits:
        sys.exit("%s：%s / %r 一行都沒命中" % (cid, stamp, needle))
    # 同一秒內連發時取最後一筆：視窗就會含著前面那幾筆，跟當下看到的畫面一致。
    i = hits[-1]
    r = rows[i]
    t = secs(r["iso"])
    win = [x for x in rows[max(0, i - 8000):i]
           if x["key"] == r["key"] and 0 <= t - secs(x["iso"]) <= WINDOW_S][-MAX_LINES:]
    hd = head(r["msg"])
    repeats = sum(1 for x in rows[max(0, i - 40000):i]
                  if x["key"] == r["key"] and head(x["msg"]) == hd and t - secs(x["iso"]) <= 3600)
    others = len({x["key"] for x in rows[max(0, i - 40000):i]
                  if head(x["msg"]) == hd and x["key"] != r["key"] and t - secs(x["iso"]) <= 300})
    ids, names = {}, {}
    for x in win + [r]:
        b = x["bot"]
        if b and not ULID.fullmatch(b):
            names.setdefault(b, "bot-%d" % (len(names) + 1))
    ev = [mask("%s %s %s" % (x["iso"][11:19], x["lvl"], x["msg"]), ids, names) for x in win]
    trig = mask("%s %s %s" % (r["iso"][11:19], r["lvl"], r["msg"]), ids, names)
    return {
        "id": cid,
        "state": {
            "source": "daemon_log", "agent_kind": kind,
            "evidence": {"window_minutes": WINDOW_S // 60, "before": ev, "trigger": trig},
            "facts": {"same_message_from_this_agent_last_hour": repeats,
                      "other_agents_with_same_message_last_5min": others,
                      "lines_about_this_agent_in_window": len(win)},
        },
        "questions": QUESTIONS,
        "labels": dict({"next_action": label}, **{q: f(label) for q, f in DERIVED.items()}),
        "meta": {"label": label, "kind": kind, "source": "daemon_log", "trigger_head": hd},
    }


def build_sup_case(cid, kind, label, what, record):
    return {
        "id": cid,
        "state": {"source": "supervisor", "agent_kind": kind,
                  "evidence": {"record_kind": what, "record": record}, "facts": {}},
        "questions": QUESTIONS,
        "labels": dict({"next_action": label}, **{q: f(label) for q, f in DERIVED.items()}),
        "meta": {"label": label, "kind": kind, "source": "supervisor", "trigger_head": what},
    }


def main():
    rows = parse(sys.argv[1])
    cases = [build_log_case(rows, *c) for c in LOG_CASES]
    cases += [build_sup_case(*c) for c in SUP_CASES]
    for c in cases:
        blob = json.dumps(c["state"], ensure_ascii=False)
        m = SECRET.search(blob)
        if m:
            sys.exit("%s：輸出裡有疑似機密的字樣 %r，不送" % (c["id"], m.group(0)))
        print(json.dumps(c, ensure_ascii=False))
    print("%d cases" % len(cases), file=sys.stderr)


if __name__ == "__main__":
    main()
