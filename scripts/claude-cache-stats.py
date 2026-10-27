#!/usr/bin/env python3
"""離線統計本機 Claude Code session jsonl 的快取用量（issue #2009 / #2008 的重量工具）。

唯讀：只讀 `~/.claude*/projects/*/*.jsonl`（含 `<sid>/subagents/*.jsonl`），`message.id + requestId` 去重。
不連 daemon、不寫任何 session 檔。輸出只有數字。

子命令：
  summary   每個專案目錄的請求數、讀／寫快取、命中率、「讀快取 ÷ 請求數」（驗收主指標）
  sessions  每個 session 的輪數、最終 context、平均每請求讀快取、>55 分鐘間隔次數與冷喚醒重寫量

選項：--days N（預設 7）、--project SUBSTR（專案目錄名包含此字串才算，可重複；
      例：--project 'supervisor-AGM$' 只量 AGM supervisor（結尾加 $ ＝ 目錄名結尾比對，
      避免連 supervisor-AGM-responder 一起算））、--root GLOB、--min-turns N、--top N。
命中率＝ read / (input + write + read)。
"""
import argparse
import collections
import datetime
import glob
import json
import os
import sys

GAP_SECS = 55 * 60  # 1h TTL 留 5 分鐘餘裕：超過就視為快取已冷
DEFAULT_ROOT = "~/.claude*/projects"


def parse_ts(s):
    try:
        return datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))
    except (ValueError, AttributeError):
        return None


def session_files(root, projects, cutoff):
    base = os.path.expanduser(root)
    for p in glob.glob(os.path.join(base, "*", "*.jsonl")) + \
            glob.glob(os.path.join(base, "*", "*", "subagents", "*.jsonl")):
        parts = p.split(os.sep)
        proj = parts[-4] if "subagents" in parts else parts[-2]
        if projects and not any(proj.endswith(s[:-1]) if s.endswith("$") else s in proj for s in projects):
            continue
        try:
            if os.path.getmtime(p) < cutoff.timestamp():
                continue
        except OSError:
            continue
        yield p, proj, "subagents" in parts


def iter_records(root, projects, cutoff):
    """yield 去重後、帶 usage 的 assistant 紀錄。"""
    seen = set()
    for path, proj, sub in session_files(root, projects, cutoff):
        try:
            fh = open(path, errors="ignore")
        except OSError:
            continue
        with fh:
            for line in fh:
                if '"usage"' not in line:
                    continue
                try:
                    d = json.loads(line)
                except ValueError:
                    continue
                m = d.get("message")
                u = m.get("usage") if isinstance(m, dict) else None
                if not u:
                    continue
                ts = parse_ts(d.get("timestamp", ""))
                if ts is None or ts < cutoff:
                    continue
                key = (m.get("id"), d.get("requestId"))
                if key != (None, None):
                    if key in seen:
                        continue
                    seen.add(key)
                yield {
                    "ts": ts, "proj": proj, "file": path, "sub": sub,
                    "in": u.get("input_tokens", 0) or 0,
                    "w": u.get("cache_creation_input_tokens", 0) or 0,
                    "r": u.get("cache_read_input_tokens", 0) or 0,
                }


def hit(i, w, r):
    t = i + w + r
    return r / t * 100 if t else 0.0


def cmd_summary(args, recs):
    by = collections.defaultdict(lambda: [0, 0, 0, 0])
    for x in recs:
        a = by[x["proj"]]
        a[0] += 1
        a[1] += x["in"]
        a[2] += x["w"]
        a[3] += x["r"]
    print(f"{'project':<62}{'reqs':>7}{'write(M)':>10}{'read(M)':>10}{'hit%':>7}{'read/req(K)':>13}")
    for k, a in sorted(by.items(), key=lambda kv: -kv[1][3])[:args.top]:
        print(f"{k[-62:]:<62}{a[0]:>7}{a[2]/1e6:>10.1f}{a[3]/1e6:>10.1f}{hit(a[1], a[2], a[3]):>7.1f}"
              f"{a[3]/a[0]/1e3:>13.1f}")
    tot = [sum(a[i] for a in by.values()) for i in range(4)]
    if tot[0]:
        print(f"{'TOTAL':<62}{tot[0]:>7}{tot[2]/1e6:>10.1f}{tot[3]/1e6:>10.1f}{hit(tot[1], tot[2], tot[3]):>7.1f}"
              f"{tot[3]/tot[0]/1e3:>13.1f}")


def cmd_sessions(args, recs):
    by = collections.defaultdict(list)
    for x in recs:
        by[x["file"]].append(x)
    rows = []
    for f, rs in by.items():
        rs.sort(key=lambda x: x["ts"])
        if len(rs) < args.min_turns:
            continue
        last = rs[-1]
        cold = [b for a, b in zip(rs, rs[1:]) if (b["ts"] - a["ts"]).total_seconds() > GAP_SECS]
        rd = sum(x["r"] for x in rs)
        rows.append({
            "sid": os.path.basename(f)[:8], "proj": rs[0]["proj"], "sub": rs[0]["sub"], "turns": len(rs),
            "ctx": last["in"] + last["r"] + last["w"], "rpr": rd / len(rs), "gaps": len(cold),
            "cold_w": sum(b["w"] for b in cold), "read": rd,
        })
    rows.sort(key=lambda r: -r["read"])
    print(f"{'project':<44}{'sid':<10}{'turns':>6}{'final_ctx(K)':>13}{'read/req(K)':>12}{'gaps>55m':>9}"
          f"{'coldW(M)':>9}{'read(M)':>9}")
    for r in rows[:args.top]:
        tag = r["proj"][-40:] + ("*" if r["sub"] else "")
        print(f"{tag:<44}{r['sid']:<10}{r['turns']:>6}{r['ctx']/1e3:>13.0f}{r['rpr']/1e3:>12.0f}"
              f"{r['gaps']:>9}{r['cold_w']/1e6:>9.1f}{r['read']/1e6:>9.1f}")
    print(f"(* ＝ subagent session；{len(rows)} 個 session（輪數 >= {args.min_turns}），依讀快取量排序，列前 {args.top})")

    print("\nfinal_ctx 分級：")
    edges = [0, 50e3, 100e3, 200e3, 300e3, 500e3, float("inf")]
    for lo, hi in zip(edges, edges[1:]):
        sel = [r for r in rows if lo <= r["ctx"] < hi]
        if sel:
            label = f"{lo/1e3:.0f}K~" + ("" if hi == float("inf") else f"{hi/1e3:.0f}K")
            print(f"  {label:<10} sessions={len(sel):>4} turns={sum(r['turns'] for r in sel):>6} "
                  f"read={sum(r['read'] for r in sel)/1e6:>8.1f}M cold_write={sum(r['cold_w'] for r in sel)/1e6:>6.1f}M")

    print("\n若 context 封頂在 N（到 N 就 compact／換 session），可省的讀快取上限＝各輪 max(0, context−N) 加總；"
          "未計收斂後的重建與摘要成本，實際會少一些：")
    total = sum(x["r"] for rs in by.values() if len(rs) >= args.min_turns for x in rs)
    for n in (100e3, 200e3, 300e3):
        saved = sum(max(0, x["in"] + x["r"] + x["w"] - n) for rs in by.values() if len(rs) >= args.min_turns for x in rs)
        print(f"  N={n/1e3:.0f}K：最多省 {saved/1e6:.0f}M／總讀 {total/1e6:.0f}M（{saved/total*100 if total else 0:.0f}%）")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["summary", "sessions"])
    ap.add_argument("--days", type=float, default=7)
    ap.add_argument("--project", action="append", default=[])
    ap.add_argument("--root", default=DEFAULT_ROOT)
    ap.add_argument("--min-turns", type=int, default=10)
    ap.add_argument("--top", type=int, default=25)
    args = ap.parse_args()
    cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=args.days)
    recs = list(iter_records(args.root, args.project, cutoff))
    print(f"# {args.cmd} days={args.days} projects={args.project or 'all'} requests={len(recs)}", file=sys.stderr)
    {"summary": cmd_summary, "sessions": cmd_sessions}[args.cmd](args, recs)


if __name__ == "__main__":
    main()
