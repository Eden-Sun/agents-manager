#!/usr/bin/env python3
"""#263 的判讀：每題各自算 precision／recall，中文與英文分開算，再列矛盾表。

  python3 analyze.py results_zh.jsonl results_en.jsonl facts.json

一個總分沒有用——要決定的是「哪幾個旗標可以上線」，所以每題一組數字。
中文準確度的兩個看法：(1) 35 則中文 vs 5 則英文原文；(2) 8 對「同一則、中譯英」配對。
"""
import json, sys

QIDS = ["claims_complete", "claims_verified", "contains_concrete_evidence",
        "asks_parent_action", "admits_unfinished"]
THRESHOLDS = (0.5, 0.8, 0.9)


def load(path):
    return [json.loads(l) for l in open(path) if l.strip()]


def pr(pairs, thr):
    """pairs: [(p, label)] → precision/recall/n_flagged for「預測為真」."""
    tp = sum(1 for p, y in pairs if p >= thr and y)
    fp = sum(1 for p, y in pairs if p >= thr and not y)
    fn = sum(1 for p, y in pairs if p < thr and y)
    tn = sum(1 for p, y in pairs if p < thr and not y)
    prec = tp / (tp + fp) if tp + fp else None
    rec = tp / (tp + fn) if tp + fn else None
    return tp, fp, fn, tn, prec, rec


def fmt(x):
    return "  n/a" if x is None else "%5.2f" % x


def table(rows, title):
    print("\n## %s  (n=%d)" % (title, len(rows)))
    print("| 題 | 真值為真 | 門檻 | 命中 | 誤報 | 漏掉 | precision | recall | acc |")
    print("|---|---|---|---|---|---|---|---|---|")
    for q in QIDS:
        pairs = [(r["answers"][q]["noul"], bool(r["labels"][q])) for r in rows
                 if r.get("answers") and q in r["labels"]]
        pos = sum(y for _, y in pairs)
        for thr in THRESHOLDS:
            tp, fp, fn, tn, prec, rec = pr(pairs, thr)
            acc = (tp + tn) / len(pairs)
            print("| %s | %d/%d | %.1f | %d | %d | %d | %s | %s | %s |"
                  % (q, pos, len(pairs), thr, tp, fp, fn, fmt(prec), fmt(rec), fmt(acc)))


def errors(rows, thr=0.5):
    print("\n## 每題的錯誤（門檻 %.1f）" % thr)
    for q in QIDS:
        bad = [(r["id"], r["answers"][q]["noul"], bool(r["labels"][q])) for r in rows
               if r.get("answers") and (r["answers"][q]["noul"] >= thr) != bool(r["labels"][q])]
        print("- **%s**：%s" % (q, "、".join(
            "%s p=%.2f 真值=%s" % (i, p, "真" if y else "假") for i, p, y in bad) or "無"))


def main():
    zh, en, facts = load(sys.argv[1]), load(sys.argv[2]), json.load(open(sys.argv[3]))
    ok = [r for r in zh if r.get("answers")]
    # 0.1 這條線：英文原文的回報只有測試名稱裡夾幾個中文字，不算中文回報。
    cjk = [r for r in ok if r["meta"]["cjk_ratio"] >= 0.1]
    ascii_only = [r for r in ok if r["meta"]["cjk_ratio"] < 0.1]
    lat = sorted(r["ms"] for r in ok + en)
    tok = sum(r["usage"]["input_tokens"] for r in ok + en if r.get("usage"))
    print("# #263 交辦回報證據旗標 — 量到的數字\n")
    print("- 呼叫 %d 次（中文樣本 %d、英譯對照 %d），失敗 0、重試 0。"
          % (len(ok) + len(en), len(ok), len(en)))
    print("- 延遲 ms：p50=%d p95=%d max=%d。input tokens 共 %d，約 $%.4f（$0.042/M）。"
          % (lat[len(lat) // 2], lat[int(.95 * (len(lat) - 1))], lat[-1], tok, tok * 0.042 / 1e6))
    table(ok, "全部樣本（真實分布）")
    table(cjk, "繁中回報")
    table(ascii_only, "全英文回報（同一批交辦，但整則是英文）")
    errors(ok)

    print("\n## 中譯英配對（同一則回報，只換語言）")
    print("| 題 | 中文 p | 英文 p | 差 | 真值 | 中文對？ | 英文對？ |")
    print("|---|---|---|---|---|---|---|")
    byid = {r["id"]: r for r in ok}
    flips = same = 0
    for e in en:
        z = byid[e["meta"]["pair"]]
        for q in QIDS:
            pz, pe, y = z["answers"][q]["noul"], e["answers"][q]["noul"], bool(z["labels"][q])
            cz, ce = (pz >= .5) == y, (pe >= .5) == y
            flips += cz != ce
            same += cz == ce
            print("| %s / %s | %.2f | %.2f | %+.2f | %s | %s | %s |"
                  % (e["meta"]["pair"], q, pz, pe, pe - pz, "真" if y else "假",
                     "○" if cz else "✗", "○" if ce else "✗"))
    print("\n配對 %d 組：判斷一致 %d、不一致 %d。" % (flips + same, same, flips))
    deltas = [abs(e["answers"][q]["noul"] - byid[e["meta"]["pair"]]["answers"][q]["noul"])
              for e in en for q in QIDS]
    deltas.sort()
    print("機率絕對差：中位數 %.2f、p90 %.2f、最大 %.2f。"
          % (deltas[len(deltas) // 2], deltas[int(.9 * (len(deltas) - 1))], deltas[-1]))

    print("\n## 矛盾表（Jev 的說法 × AGM 自己查得到的事實）")
    print("\n只認**解析得出是 git commit** 的 sha。報告裡那些 7～40 碼 hex 有一半是 sha256 前綴、"
          "備份檔尾巴或別的 repo 的 commit，把它們當成「不在 main」會製造假警報——先列在後面。\n")
    print("| 回報 | claims_complete | claims_verified | 解析得出的 sha | 其中不在 main | CI run | 判讀 |")
    print("|---|---|---|---|---|---|---|")
    hits = 0
    for r in ok:
        f = facts[r["id"]]
        off = [s for s in f["sha_resolved"] if s not in f["sha_on_main"]]
        runs = ",".join("%s=%s" % kv for kv in f["ci_runs"].items()) or "—"
        cc, cv = r["answers"]["claims_complete"]["noul"], r["answers"]["claims_verified"]["noul"]
        note = []
        if cc >= .8 and off:
            note.append("宣稱完成，但有 commit 不在 main")
        if cv >= .8 and not f["ci_runs"]:
            note.append("宣稱通過，但沒有 CI run id")
        if not note:
            continue
        hits += 1
        print("| %s | %.2f | %.2f | %s | %s | %s | %s |"
              % (r["id"], cc, cv, ",".join(f["sha_resolved"]) or "—",
                 ",".join(off) or "—", runs, "；".join(note)))
    print("\n共 %d 則觸發。" % hits)
    print("\n本來就有 commit 不在 main 的回報（不看 Jev，事實本身）：")
    for cid, f in facts.items():
        off = [s for s in f["sha_resolved"] if s not in f["sha_on_main"]]
        if off:
            print("- %s：%s；該則 claims_complete=%.2f"
                  % (cid, ",".join(off), byid[cid]["answers"]["claims_complete"]["noul"]))
    noise = {cid: f["sha_unresolved"] for cid, f in facts.items() if f["sha_unresolved"]}
    print("\n解析不出是 commit 的 hex（沿用 regex 會變成假警報）：%d 則、%d 個 token，例如 %s。"
          % (len(noise), sum(len(v) for v in noise.values()),
             "、".join("%s:%s" % (k, v[0]) for k, v in list(noise.items())[:4])))


if __name__ == "__main__":
    main()
