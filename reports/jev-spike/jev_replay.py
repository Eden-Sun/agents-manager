#!/usr/bin/env python3
"""#240 離線重放：把 JSONL 題目丟給 TypeSafe Jev，寫回 JSONL 答案與統計。

不接 daemon。key 只從環境變數 TYPESAFE_API_KEY 讀，不寫進任何輸出。
  TYPESAFE_API_KEY="$(cat ~/.config/typesafe/api-key)" python3 jev_replay.py run cases_a.jsonl results_a.jsonl
  python3 jev_replay.py report results_a.jsonl

cases 每列：{"id", "state", "questions", "labels": {qid: 標準答案}, "meta": {...}}
  noul 的標準答案是 true/false；choice 是選項名。
"""
import json
import os
import ssl
import statistics
import sys
import time
import urllib.error
import urllib.request

URL = "https://api.typesafe.ai/v1/systemone"
MODEL = "jev-1.13.0"
MAX_CALLS = 50
TIMEOUT = 30
# python.org 的 macOS build 沒帶 CA；用系統那份，不關驗證。
CTX = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if os.path.exists("/etc/ssl/cert.pem") else None)


def call(body, key):
    data = json.dumps(body).encode()
    retries = 0
    while True:
        req = urllib.request.Request(URL, data=data, method="POST", headers={
            "Authorization": "Bearer " + key, "Content-Type": "application/json"})
        t0 = time.monotonic()
        try:
            with urllib.request.urlopen(req, timeout=TIMEOUT, context=CTX) as r:
                return json.loads(r.read()), (time.monotonic() - t0) * 1000, retries, None
        except urllib.error.HTTPError as e:
            if e.code in (429, 529) and retries < 5:
                retries += 1
                time.sleep(min(60, 2 ** retries))
                continue
            # 只留狀態碼與回應本文開頭；不碰 request header。
            return None, (time.monotonic() - t0) * 1000, retries, "http %s: %s" % (e.code, e.read()[:300].decode("utf-8", "replace"))
        except Exception as e:  # noqa: BLE001 逾時、連線錯誤都算一次失敗，不重試
            return None, (time.monotonic() - t0) * 1000, retries, "%s: %s" % (type(e).__name__, e)


def run(cases_path, out_path):
    key = os.environ.get("TYPESAFE_API_KEY", "").strip()
    if not key:
        sys.exit("TYPESAFE_API_KEY 沒設")
    cases = [json.loads(l) for l in open(cases_path) if l.strip()]
    if len(cases) > MAX_CALLS:
        sys.exit("超過 %d 筆，先砍樣本" % MAX_CALLS)
    with open(out_path, "w") as out:
        for c in cases:
            resp, ms, retries, err = call({"model": MODEL, "state": c["state"], "questions": c["questions"]}, key)
            row = {"id": c["id"], "labels": c.get("labels", {}), "meta": c.get("meta", {}),
                   "ms": round(ms, 1), "retries": retries, "error": err,
                   "model": resp and resp.get("model"), "usage": resp and resp.get("usage"),
                   "answers": resp and resp.get("answers")}
            out.write(json.dumps(row, ensure_ascii=False) + "\n")
            out.flush()
            print(c["id"], "ERR " + err if err else "%.0fms" % ms, file=sys.stderr)
            time.sleep(0.2)


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p * (len(xs) - 1))))] if xs else None


def report(path, group_key=None):
    rows = [json.loads(l) for l in open(path) if l.strip()]
    ok = [r for r in rows if not r["error"]]
    print("calls=%d ok=%d errors=%d retries=%d" % (len(rows), len(ok), len(rows) - len(ok), sum(r["retries"] for r in rows)))
    for r in rows:
        if r["error"]:
            print("  error", r["id"], r["error"])
    ms = [r["ms"] for r in ok]
    if ms:
        print("latency ms p50=%.0f p95=%.0f max=%.0f" % (pct(ms, .5), pct(ms, .95), max(ms)))
        tin = [r["usage"]["input_tokens"] for r in ok if r.get("usage")]
        print("input_tokens total=%d median=%d max=%d  cost=$%.5f" % (sum(tin), statistics.median(tin), max(tin), sum(tin) * 0.042 / 1e6))
    groups = {}
    for r in ok:
        g = str(r["meta"].get(group_key)) if group_key else "all"
        groups.setdefault(g, []).append(r)
    for g, rs in sorted(groups.items()):
        print("== group", g, "n=%d" % len(rs))
        qids = sorted({q for r in rs for q in r["labels"]})
        for q in qids:
            pairs = [(r, r["answers"][q]) for r in rs if q in r["labels"] and q in (r["answers"] or {})]
            if not pairs:
                continue
            if pairs[0][1]["type"] == "noul":
                # (預測機率, 真值)
                pv = [(a["noul"], bool(r["labels"][q])) for r, a in pairs]
                acc = sum((p >= .5) == y for p, y in pv) / len(pv)
                brier = sum((p - y) ** 2 for p, y in pv) / len(pv)
                print("  %s noul n=%d acc@0.5=%.2f brier=%.3f" % (q, len(pv), acc, brier))
                for lo, hi in ((0, .2), (.2, .4), (.4, .6), (.6, .8), (.8, 1.01)):
                    b = [y for p, y in pv if lo <= p < hi]
                    if b:
                        print("     p∈[%.1f,%.1f) n=%d 實際為真=%.2f" % (lo, min(hi, 1), len(b), sum(b) / len(b)))
                for r, a in pairs:
                    if (a["noul"] >= .5) != bool(r["labels"][q]):
                        print("     ✗ %s p=%.2f label=%s" % (r["id"], a["noul"], r["labels"][q]))
            else:
                hit = [(a["choice"] == r["labels"][q], a.get("confidence", 0), r, a) for r, a in pairs]
                print("  %s choice n=%d acc=%.2f" % (q, len(hit), sum(h[0] for h in hit) / len(hit)))
                for lo, hi in ((0, .5), (.5, .8), (.8, 1.01)):
                    b = [h[0] for h in hit if lo <= h[1] < hi]
                    if b:
                        print("     conf∈[%.1f,%.1f) n=%d acc=%.2f" % (lo, min(hi, 1), len(b), sum(b) / len(b)))
                for h in hit:
                    if not h[0]:
                        print("     ✗ %s got=%s conf=%.2f label=%s" % (h[2]["id"], h[3]["choice"], h[1], h[2]["labels"][q]))


if __name__ == "__main__":
    if len(sys.argv) >= 4 and sys.argv[1] == "run":
        run(sys.argv[2], sys.argv[3])
    elif len(sys.argv) >= 3 and sys.argv[1] == "report":
        report(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None)
    else:
        sys.exit(__doc__)
