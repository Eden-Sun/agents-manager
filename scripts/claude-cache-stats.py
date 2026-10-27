#!/usr/bin/env python3
"""離線唯讀 Claude JSONL 統計；不呼叫模型、不修改 session。

summary / sessions 與 --days、--project、--root、--min-turns、--top 保留；
--project 仍是目錄名 substring，結尾 $ 為 suffix。--root 為 projects glob。
--since / --until 可固定 UTC 窗口 [since, until)，覆蓋 --days 的起點／現在終點。
無時區 timestamp 不猜時區，計入 invalid_timestamp。usage 缺值不是零；
重複 message.id + requestId 的 partial usage 合併各欄最大已知值。
無 requestId 時以 message.id 去重並計數；無 message.id 的 usage 不計請求。
user_turns 只計非 meta 的 user 文字（排 tool_result），uuid 去重，無 uuid 不猜。

原 project / sid 欄位現在為安全 signature（p-/s-）；已知 supervisor-AGM、
supervisor-AGM-responder 使用固定別名，不輸出路徑、prompt 或 tool body。
turns / --min-turns 保留原語意：API requests，並非 user turns。
各列 user_turns 限該列有請求的 session；quality.user_turns 包含全窗口文字 turn。
reqs 是全部唯一 request；read/req、hit%、context 僅以完整 usage 的 measured
為 denominator。未知顯示 NA；新增 p50/p95 為 nearest-rank。
gaps>55m / coldW 是間隔粗估，不是 cache 失效證明。理論截斷量不是實測節省，
未計摘要、重建、額外請求；不修改 compact 或 session 政策。

只讀開啟時固定檔案長度快照。--max-files/bytes/line-bytes/records 限制工作量；
超限、讀錯、空檔案集合回 2，且不輸出不完整的統計；malformed 明列計數。
"""
import argparse
import collections
import datetime as dt
import glob
import hashlib
import json
import math
import os
import stat
import sys

GAP_SECS = 55 * 60
DEFAULT_ROOT = "~/.claude*/projects"
UTC = dt.timezone.utc
FIELDS = (("in", "input_tokens"), ("w", "cache_creation_input_tokens"),
          ("r", "cache_read_input_tokens"))


class AuditError(Exception):
    pass


def signature(value, prefix):
    return prefix + hashlib.sha256(value.encode("utf-8", "replace")).hexdigest()[:12]


def project_tag(value):
    for tag in ("supervisor-AGM-responder", "supervisor-AGM"):
        if value.endswith("-" + tag):
            return tag
    return signature(value, "p-")


def parse_ts(value):
    try:
        stamp = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
        return stamp.astimezone(UTC) if stamp.tzinfo is not None else None
    except (ValueError, AttributeError, OverflowError):
        return None


def positive_int(value):
    try:
        number = int(value)
    except ValueError:
        raise argparse.ArgumentTypeError("requires a positive integer") from None
    if number <= 0:
        raise argparse.ArgumentTypeError("requires a positive integer")
    return number


def positive_float(value):
    try:
        number = float(value)
    except ValueError:
        raise argparse.ArgumentTypeError("requires a positive finite number") from None
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("requires a positive finite number")
    return number


def session_files(root, projects, max_files):
    base = os.path.expanduser(root)
    seen = set()
    count = 0
    for pattern in ("*/*.jsonl", "*/*/subagents/*.jsonl"):
        for path in glob.iglob(os.path.join(base, pattern)):
            count += 1
            if count > max_files:
                raise AuditError("max-files exceeded")
            parts = path.split(os.sep)
            sub = pattern.startswith("*/*/")
            project = parts[-4] if sub else parts[-2]
            if projects and not any(project.endswith(s[:-1]) if s.endswith("$") else s in project
                                    for s in projects):
                continue
            try:
                info = os.stat(path, follow_symlinks=False)
            except OSError:
                raise AuditError("file stat failed") from None
            if not stat.S_ISREG(info.st_mode):
                raise AuditError("non-regular input")
            identity = (info.st_dev, info.st_ino)
            if identity in seen:
                continue
            seen.add(identity)
            yield path, project_tag(project), sub


def user_text(record, message):
    if record.get("type") != "user" or record.get("isMeta") or message.get("role") != "user":
        return False
    content = message.get("content")
    if isinstance(content, str):
        return bool(content.strip())
    if not isinstance(content, list):
        return False
    # A tool response may include explanatory text; it is still not a user turn.
    if any(isinstance(x, dict) and x.get("type") == "tool_result" for x in content):
        return False
    return any(isinstance(x, dict) and x.get("type") == "text"
               and isinstance(x.get("text"), str) and x["text"].strip() for x in content)


def collect(args, since, until):
    quality = collections.Counter()
    requests = {}
    users = collections.defaultdict(set)
    user_keys = set()
    for path, project, sub in session_files(args.root, args.project, args.max_files):
        session = signature(project + "\0" + os.path.basename(path), "s-")
        try:
            fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
            with os.fdopen(fd, "rb") as stream:
                info = os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode):
                    raise AuditError("non-regular input")
                remaining = info.st_size
                quality["files"] += 1
                if quality["bytes"] + remaining > args.max_bytes:
                    raise AuditError("max-bytes exceeded")
                quality["bytes"] += remaining
                while remaining:
                    raw = stream.readline(min(remaining, args.max_line_bytes + 1))
                    if not raw:
                        raise AuditError("file truncated during snapshot")
                    remaining -= len(raw)
                    if len(raw) > args.max_line_bytes:
                        raise AuditError("max-line-bytes exceeded")
                    try:
                        record = json.loads(raw)
                    except (ValueError, UnicodeError, RecursionError):
                        quality["malformed"] += 1
                        continue
                    if not isinstance(record, dict):
                        quality["malformed"] += 1
                        continue
                    stamp = parse_ts(record.get("timestamp"))
                    if stamp is None:
                        quality["invalid_timestamp"] += 1
                        continue
                    if not since <= stamp < until:
                        continue
                    message = record.get("message")
                    if not isinstance(message, dict):
                        continue
                    if user_text(record, message):
                        uid = record.get("uuid")
                        if not isinstance(uid, str) or not uid:
                            quality["user_missing_uuid"] += 1
                        else:
                            uid = signature(uid, "u-")
                            key = (project, uid)
                            if key in user_keys:
                                quality["user_duplicate"] += 1
                            else:
                                if len(user_keys) >= args.max_records:
                                    raise AuditError("max-records user turns exceeded")
                                user_keys.add(key)
                                users[session].add(uid)
                    usage = message.get("usage")
                    if (not isinstance(usage, dict) or record.get("type") not in (None, "assistant")
                            or message.get("role") not in (None, "assistant")):
                        continue
                    mid, rid = message.get("id"), record.get("requestId")
                    if not isinstance(mid, str) or not mid:
                        quality["missing_message_id"] += 1
                        continue
                    if not isinstance(rid, str) or not rid:
                        quality["missing_request_id"] += 1
                        rid = None
                    key = (project, signature(mid, "m-"), signature(rid, "r-") if rid else None)
                    if key not in requests:
                        if len(requests) >= args.max_records:
                            raise AuditError("max-records requests exceeded")
                        requests[key] = dict(ts=stamp, proj=project, session=session, sub=sub,
                                             **{name: None for name, _ in FIELDS})
                    else:
                        quality["duplicate"] += 1
                    target = requests[key]
                    target["ts"] = min(stamp, target["ts"])
                    for name, field in FIELDS:
                        value = usage.get(field)
                        if type(value) is int and 0 <= value <= 2**63 - 1:
                            target[name] = max(value, target[name] or 0)
                        else:
                            quality["unknown_fields"] += 1
        except OSError:
            raise AuditError("file read failed") from None
    if not quality["files"]:
        raise AuditError("no matching regular input files")
    records = list(requests.values())
    quality["unknown_requests"] = sum(not complete(x) for x in records)
    quality["user_turns"] = len(user_keys)
    return records, users, quality


def complete(record):
    return all(record[name] is not None for name, _ in FIELDS)


def context(record):
    return sum(record[name] for name, _ in FIELDS)


def percentile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)] if values else None


def number(value, scale=1, digits=1):
    return "NA" if value is None else f"{value / scale:.{digits}f}"


def metrics(records):
    measured = [x for x in records if complete(x)]
    total = {name: sum(x[name] for x in measured) for name, _ in FIELDS}
    contexts = [context(x) for x in measured]
    return measured, total, contexts


def cmd_summary(args, records, users):
    by = collections.defaultdict(list)
    for record in records:
        by[record["proj"]].append(record)
    print(f"{'project':<62}{'reqs':>7}{'write(M)':>10}{'read(M)':>10}{'hit%':>7}{'read/req(K)':>13}"
          " measured ctx_p50(K) ctx_p95(K) user_turns")

    def row(tag, rs):
        measured, totals, contexts = metrics(rs)
        denominator = sum(totals.values())
        rate = totals["r"] / denominator * 100 if denominator else (0 if measured else None)
        sessions = {x["session"] for x in rs}
        turns = sum(len(users[s]) for s in sessions)
        print(f"{tag:<62}{len(rs):>7}{number(totals['w'] if measured else None, 1e6):>10}"
              f"{number(totals['r'] if measured else None, 1e6):>10}{number(rate):>7}"
              f"{number(totals['r']/len(measured) if measured else None, 1e3):>13}"
              f" {len(measured)} {number(percentile(contexts, .5), 1e3)}"
              f" {number(percentile(contexts, .95), 1e3)} {turns}")
    for project, rs in sorted(by.items(), key=lambda item: -metrics(item[1])[1]["r"])[:args.top]:
        row(project, rs)
    if records:
        row("TOTAL", records)


def cmd_sessions(args, records, users):
    by = collections.defaultdict(list)
    for record in records:
        by[record["session"]].append(record)
    rows = []
    for session, rs in by.items():
        rs.sort(key=lambda x: x["ts"])
        if len(rs) < args.min_turns:
            continue
        measured, total, contexts = metrics(rs)
        cold = [b for a, b in zip(rs, rs[1:]) if (b["ts"]-a["ts"]).total_seconds() > GAP_SECS]
        rows.append(dict(sid=session, proj=rs[0]["proj"], sub=rs[0]["sub"], turns=len(rs),
                         ctx=context(rs[-1]) if complete(rs[-1]) else None,
                         rpr=total["r"]/len(measured) if measured else None,
                         gaps=len(cold), cold_w=sum(b["w"] for b in cold if b["w"] is not None),
                         cold_known=sum(b["w"] is not None for b in cold),
                         read=total["r"], measured=len(measured), contexts=contexts, records=rs,
                         user_turns=len(users[session])))
    rows.sort(key=lambda row: -row["read"])
    print(f"{'project':<44}{'sid':<16}{'turns':>6}{'final_ctx(K)':>13}{'read/req(K)':>12}"
          f"{'gaps>55m':>9}{'coldW(M)':>9}{'read(M)':>9}"
          " measured user_turns ctx_p50(K) ctx_p95(K) cold_known")
    for row in rows[:args.top]:
        tag = row["proj"] + ("*" if row["sub"] else "")
        print(f"{tag:<44}{row['sid']:<16}{row['turns']:>6}{number(row['ctx'],1e3,0):>13}"
              f"{number(row['rpr'],1e3,0):>12}{row['gaps']:>9}"
              f"{number(row['cold_w'] if row['cold_known'] == row['gaps'] else None,1e6):>9}"
              f"{number(row['read'] if row['measured'] else None,1e6):>9}"
              f" {row['measured']} {row['user_turns']}"
              f" {number(percentile(row['contexts'],.5),1e3)}"
              f" {number(percentile(row['contexts'],.95),1e3)} {row['cold_known']}")
    print(f"# sessions={len(rows)} min_requests={args.min_turns} top={args.top}; "
          "turns=API_requests; *=subagent; cold_gap=粗估、不是cache失效證明")
    for name in ("turns", "user_turns"):
        values = [row[name] for row in rows]
        print(f"# session_{name} measured={len(values)} p50={number(percentile(values,.5),digits=0)}"
              f" p95={number(percentile(values,.95),digits=0)}")
    print("\nfinal_ctx 分級：")
    edges = [0, 50e3, 100e3, 200e3, 300e3, 500e3, float("inf")]
    for lo, hi in zip(edges, edges[1:]):
        selected = [r for r in rows if r["ctx"] is not None and lo <= r["ctx"] < hi]
        if selected:
            label = f"{lo/1e3:.0f}K~" + ("" if math.isinf(hi) else f"{hi/1e3:.0f}K")
            print(f"  {label:<10} sessions={len(selected)} turns={sum(r['turns'] for r in selected)}"
                  f" read={sum(r['read'] for r in selected)/1e6:.1f}M"
                  f" cold_write={number(sum(r['cold_w'] for r in selected) if all(r['cold_known'] == r['gaps'] for r in selected) else None, 1e6)}M")
    contexts = [value for row in rows for value in row["contexts"]]
    print("\n理論截斷量、不是實測節省（未計摘要／重建／額外請求，非read或成本預測）：")
    for threshold in (100e3, 200e3, 300e3):
        truncated = sum(max(0, value-threshold) for value in contexts)
        print(f"  N={threshold/1e3:.0f}K theoretical_context_truncation={truncated/1e6:.1f}M"
              f" measured={len(contexts)}")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["summary", "sessions"])
    ap.add_argument("--days", type=positive_float, default=7)
    ap.add_argument("--project", action="append", default=[])
    ap.add_argument("--root", default=DEFAULT_ROOT)
    ap.add_argument("--min-turns", type=positive_int, default=10)
    ap.add_argument("--top", type=positive_int, default=25)
    ap.add_argument("--since")
    ap.add_argument("--until")
    ap.add_argument("--max-files", type=positive_int, default=20000)
    ap.add_argument("--max-bytes", type=positive_int, default=8*1024**3)
    ap.add_argument("--max-line-bytes", type=positive_int, default=8*1024**2)
    ap.add_argument("--max-records", type=positive_int, default=500000)
    args = ap.parse_args(argv)
    until = parse_ts(args.until) if args.until else dt.datetime.now(UTC)
    try:
        since = parse_ts(args.since) if args.since else until - dt.timedelta(days=args.days)
    except (OverflowError, TypeError):
        since = None
    if since is None or until is None or since >= until:
        ap.error("requires valid timezone-aware since < until")
    try:
        records, users, quality = collect(args, since, until)
    except AuditError as exc:
        print(f"error: {exc}; incomplete=1", file=sys.stderr)
        return 2
    print(f"# {args.cmd} since={since.isoformat()} until={until.isoformat()} requests={len(records)}",
          file=sys.stderr)
    names = ("files", "bytes", "malformed", "invalid_timestamp", "duplicate", "unknown_fields",
             "unknown_requests", "missing_message_id", "missing_request_id", "user_duplicate",
             "user_missing_uuid", "user_turns")
    print("# quality " + " ".join(f"{key}={quality[key]}" for key in names), file=sys.stderr)
    {"summary": cmd_summary, "sessions": cmd_sessions}[args.cmd](args, records, users)
    return 0


if __name__ == "__main__":
    sys.exit(main())
