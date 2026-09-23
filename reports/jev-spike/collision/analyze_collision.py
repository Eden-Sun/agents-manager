#!/usr/bin/env python3
"""#264 的分組指標：語言、確定性路徑比對的對照、每個候選的 top-k 排名。

用法：analyze_collision.py results_c.jsonl
"""
import json
import statistics
import sys

rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
ok = [r for r in rows if not r["error"]]
for r in ok:
    r["pred"] = r["answers"]["collision"]["score"]
    r["conf"] = r["answers"]["collision"]["confidence"]
    r["noul"] = r["answers"]["same_work"]["noul"]
    r["y"] = r["meta"]["label"]
    r["pos"] = r["meta"]["positive"]

THRESHOLD = 1.5  # score ≥ 1.5 就提示「可能撞題」


def prf(flagged, rs):
    tp = sum(1 for r in rs if flagged(r) and r["pos"])
    fp = sum(1 for r in rs if flagged(r) and not r["pos"])
    fn = sum(1 for r in rs if not flagged(r) and r["pos"])
    tn = sum(1 for r in rs if not flagged(r) and not r["pos"])
    prec = tp / (tp + fp) if tp + fp else float("nan")
    rec = tp / (tp + fn) if tp + fn else float("nan")
    fpr = fp / (fp + tn) if fp + tn else float("nan")
    return tp, fp, fn, tn, prec, rec, fpr


def line(name, rs, flagged):
    tp, fp, fn, tn, prec, rec, fpr = prf(flagged, rs)
    mae = sum(abs(r["pred"] - r["y"]) for r in rs) / len(rs)
    print("  %-26s n=%2d tp=%d fp=%d fn=%d tn=%2d precision=%.2f recall=%.2f 誤報率=%.2f mae=%.2f"
          % (name, len(rs), tp, fp, fn, tn, prec, rec, fpr, mae))


print("== 提示門檻 score ≥ %.1f（正例＝標籤 2 或 3）" % THRESHOLD)
flag = lambda r: r["pred"] >= THRESHOLD
line("全部", ok, flag)
for lang in ("zh", "en"):
    line("語言=%s" % lang, [r for r in ok if r["meta"]["lang"] == lang], flag)
line("既有是進行中交辦", [r for r in ok if r["meta"]["in_flight"]], flag)
line("既有是已開的 issue", [r for r in ok if not r["meta"]["in_flight"]], flag)

print("== 同一條線改用 noul ≥ 0.5")
line("全部", ok, lambda r: r["noul"] >= .5)
for lang in ("zh", "en"):
    line("語言=%s" % lang, [r for r in ok if r["meta"]["lang"] == lang], lambda r: r["noul"] >= .5)

print("== 確定性路徑比對（票面提到同一個原始檔）當成唯一判斷")
line("路徑比對", ok, lambda r: r["meta"]["path_overlap"])
print("  只有路徑重疊的正例 %d／%d；路徑沒重疊但 Jev 抓到的正例 %d"
      % (sum(1 for r in ok if r["pos"] and r["meta"]["path_overlap"]),
         sum(1 for r in ok if r["pos"]),
         sum(1 for r in ok if r["pos"] and not r["meta"]["path_overlap"] and flag(r))))
print("  路徑重疊但其實不撞題（誤報） %s"
      % [r["id"] for r in ok if r["meta"]["path_overlap"] and not r["pos"]])

print("== 每個候選對 5 個既有項目排名（top-k：正例有沒有排進前 k）")
groups = {}
for r in ok:
    groups.setdefault(r["meta"]["candidate"], []).append(r)
hits = {1: 0, 2: 0}
ranked = 0
for cand, rs in sorted(groups.items()):
    if len(rs) < 3:
        continue
    ranked += 1
    rs = sorted(rs, key=lambda r: -r["pred"])
    order = " > ".join("%s(%.2f%s)" % (r["meta"]["existing"], r["pred"], "*" if r["pos"] else "") for r in rs)
    for k in (1, 2):
        if any(r["pos"] for r in rs[:k]):
            hits[k] += 1
    print("  #%-4d %s" % (cand, order))
print("  候選組 n=%d  top-1 命中=%.2f  top-2 命中=%.2f（* 是人工標的正例）"
      % (ranked, hits[1] / ranked, hits[2] / ranked))

print("== 校準（把 score 的機率質量 P(level≥2) 當成撞題機率）")
pv = []
for r in ok:
    p = r["answers"]["collision"]["probabilities"]
    pv.append((p.get("2", 0) + p.get("3", 0), r["pos"]))
brier = sum((p - y) ** 2 for p, y in pv) / len(pv)
print("  brier=%.3f  acc@0.5=%.2f" % (brier, sum((p >= .5) == y for p, y in pv) / len(pv)))
for lo, hi in ((0, .2), (.2, .4), (.4, .6), (.6, .8), (.8, 1.01)):
    b = [y for p, y in pv if lo <= p < hi]
    if b:
        print("     P∈[%.1f,%.1f) n=%2d 實際撞題=%.2f" % (lo, min(hi, 1), len(b), sum(b) / len(b)))

print("== 成本與延遲")
ms = sorted(r["ms"] for r in ok)
tin = [r["usage"]["input_tokens"] for r in ok]
print("  n=%d  延遲 p50=%.0f p95=%.0f max=%.0f ms" % (len(ok), ms[len(ms) // 2], ms[int(.95 * (len(ms) - 1))], ms[-1]))
print("  input tokens 中位=%d max=%d 總計=%d  花費=$%.5f（每題 $%.6f）"
      % (statistics.median(tin), max(tin), sum(tin), sum(tin) * 0.042 / 1e6, statistics.median(tin) * 0.042 / 1e6))

print("== 標錯的題（|Δ|≥0.6 或 noul 翻面）")
for r in sorted(ok, key=lambda r: -abs(r["pred"] - r["y"])):
    if abs(r["pred"] - r["y"]) >= .6 or (r["noul"] >= .5) != r["pos"]:
        print("  %-12s label=%d 預測=%.2f conf=%.2f noul=%.2f  %s"
              % (r["id"], r["y"], r["pred"], r["conf"], r["noul"], r["meta"]["why"][:70]))
