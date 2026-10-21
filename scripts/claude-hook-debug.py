#!/usr/bin/env python3
"""離線統計 Claude Code `--debug` log 裡的 hook 耗時（Claude Code 2.1.296+）。

只讀使用者明確給的檔案（或 `-` ＝ stdin）；不自己找 ~/.claude/debug、不讀 session、不開 --debug。
格式依據官方文件 https://code.claude.com/docs/en/hooks#debug-hooks ：

    Hooks: PostToolUse:Write [.claude/hooks/log-write.sh] finished with status 0 (31ms)

結束狀態：`finished with status N (Xms)`、`timed out after Nms`、`cancelled`、`moved to the background`、
`failed to start`；Notification／SessionEnd／PreCompact 等是沒有耗時的 `completed with status N`。

輸出只有 event（白名單）× outcome（固定集合）的次數與耗時統計。**不輸出**原始行、command、plugin、
matcher、參數：這些是任意文字，不保證不含敏感內容。看不懂的行一律 skip；沒有耗時的行不算 0。
這只反映 Claude 眼中 hook 本身的耗時，**不能**區分 MCP 或模型慢。
"""
import json
import re
import sys

MAX_BYTES = 64 * 1024 * 1024  # 讀入總量上限
MAX_LINE = 8 * 1024  # 單行上限，超過整行 skip
MAX_SAMPLES = 10_000  # 每個群組留幾筆耗時（只用來算百分位）；超過後百分位來自前 N 筆，次數與 max 仍是全體
MAX_MS = 24 * 3600 * 1000  # 超過一天的耗時視為不可信，整行 skip

# 白名單：event 名稱不在這裡的行 skip（輸出不會有任意文字）。
EVENTS = frozenset("""PreToolUse PostToolUse PostToolUseFailure PermissionRequest PermissionDenied UserPromptSubmit
SessionStart Setup Stop StopFailure SubagentStart SubagentStop Notification SessionEnd PreCompact PostCompact
TeammateIdle TaskCreated TaskCompleted Elicitation ElicitationResult ConfigChange InstructionsLoaded
CwdChanged FileChanged WorktreeCreate WorktreeRemove""".split())

# 行尾的結束狀態（錨在行尾：中間的 command／plugin／matcher 是任意文字，不去解析）。
# 數字最多 9 位：更長的不是正常值，整行 skip（也避開 Python int() 對超長數字串的 ValueError）。
_TAIL = re.compile(
    r"(?P<outcome>finished with status (?P<fs>\d{1,9})|completed with status (?P<cs>\d{1,9})"
    r"|timed out after \d{1,9}ms|cancelled|moved to the background|failed to start)"
    r"(?: \((?P<ms>\d{1,9})ms\))?\s*$"
)
_HEAD = re.compile(r"Hooks: (?P<event>[A-Za-z]+)(?::\S*)? \[")
_OUTCOME = {"finished": "finished", "completed": "completed", "timed": "timed_out", "cancelled": "cancelled",
            "moved": "backgrounded", "failed": "failed_to_start"}


def parse_line(text):
    """回 (event, outcome, status|None, duration_ms|None)；看不懂回 None。"""
    h = _HEAD.search(text)
    if not h or h["event"] not in EVENTS:
        return None
    t = _TAIL.search(text, h.end())
    if not t:
        return None
    outcome = _OUTCOME[t["outcome"].split()[0]]
    status = t["fs"] or t["cs"]
    # 只有 finished 的括號耗時才算耗時；timed out 的 N 是上限不是實測，不混進統計。
    ms = int(t["ms"]) if t["ms"] and outcome == "finished" else None
    if ms is not None and ms > MAX_MS:
        return None
    return h["event"], outcome, int(status) if status else None, ms


def _pct(sorted_vals, p):
    # 最近鄰：回真的發生過的樣本。
    return sorted_vals[min(len(sorted_vals) - 1, max(0, -(-len(sorted_vals) * p // 100) - 1))]


def summarize(stream):
    groups = {}
    lines = skipped = overlong = 0
    read = 0
    truncated = False
    while True:
        raw = stream.readline(MAX_LINE + 1)
        if not raw:
            break
        read += len(raw)
        if read > MAX_BYTES:
            truncated = True
            break
        if len(raw) > MAX_LINE and not raw.endswith(b"\n"):
            overlong += 1
            while not truncated:  # 把這一行剩下的丟掉；每個 chunk 都先查總量，沒有換行的串流也停得下來
                rest = stream.readline(MAX_LINE)
                read += len(rest)
                if read > MAX_BYTES:
                    truncated = True
                elif not rest or rest.endswith(b"\n"):
                    break
            if truncated:
                break
            continue
        lines += 1
        got = parse_line(raw.decode("utf-8", "replace"))
        if got is None:
            skipped += 1
            continue
        event, outcome, status, ms = got
        g = groups.setdefault((event, outcome), {"count": 0, "nonzero_status": 0, "ms": [], "n": 0, "max": 0})
        g["count"] += 1
        if status:
            g["nonzero_status"] += 1
        if ms is not None:
            g["n"] += 1
            g["max"] = max(g["max"], ms)  # 全體 max：串流累積，常數記憶體
            if len(g["ms"]) < MAX_SAMPLES:
                g["ms"].append(ms)
    out = []
    for (event, outcome), g in sorted(groups.items()):
        row = {"event": event, "outcome": outcome, "count": g["count"], "nonzero_status": g["nonzero_status"],
               "with_duration": g["n"]}
        if g["n"]:
            sm = sorted(g["ms"])
            row.update(p50_ms=_pct(sm, 50), p95_ms=_pct(sm, 95), max_ms=g["max"])
            if g["n"] > len(sm):  # 百分位只來自前 MAX_SAMPLES 筆；max 與 with_duration 是全體
                row["percentile_sample"] = len(sm)
        out.append(row)
    return {"lines": lines, "skipped_lines": skipped, "overlong_lines": overlong, "bytes_read": read,
            "truncated": truncated, "groups": out,
            "note": "Claude 眼中 hook 本身的耗時；不能區分 MCP 或模型慢。沒有耗時的行不計入百分位。"}


def main(argv):
    if len(argv) != 1 or argv[0] in ("-h", "--help"):
        print("用法：claude-hook-debug.py <debug 檔路徑 | ->  （- ＝ stdin）", file=sys.stderr)
        return 2
    try:
        if argv[0] == "-":
            result = summarize(sys.stdin.buffer)
        else:
            with open(argv[0], "rb") as f:
                result = summarize(f)
    except OSError as e:
        print(f"claude-hook-debug: 讀不到輸入：{e.strerror}", file=sys.stderr)
        return 2
    json.dump(result, sys.stdout, ensure_ascii=False, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
