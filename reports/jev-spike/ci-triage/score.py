#!/usr/bin/env python3
"""#266 計分：混淆矩陣、每類 precision/recall、校準，以及「重跑一次」政策的精確率。

    python3 score.py            # 讀同目錄的 cases_*.jsonl / results_*.jsonl

比較三件事：
  1. choice 一題五選一
  2. noul 三題（其中 rerun_would_pass 是真正要的那個決定）
  3. 只看歷史的土法：這次紅的測試在上一個 run 還是綠的 → 才建議重跑
"""
import json
import os
import sys

D = os.path.dirname(os.path.abspath(__file__))
CLASSES = ["real_regression", "runner_environment", "flaky_timing", "infra", "unclear"]


def load(name):
    return {json.loads(l)["id"]: json.loads(l) for l in open(f"{D}/{name}") if l.strip()}


def table(rows, cols, get, width=20):
    out = ["%-*s %s" % (width, "", " ".join("%-18s" % c[:18] for c in cols))]
    for r in rows:
        out.append("%-*s %s" % (width, r[:width], " ".join("%-18s" % get(r, c) for c in cols)))
    return "\n".join(out)


def main():
    cases_c, res_c = load("cases_choice.jsonl"), load("results_choice.jsonl")
    cases_n, res_n = load("cases_noul.jsonl"), load("results_noul.jsonl")
    ids = list(cases_c)
    truth = {i: cases_c[i]["labels"]["failure_class"] for i in ids}
    meta = {i: cases_c[i]["meta"] for i in ids}
    pred_c = {i: res_c[i]["answers"]["failure_class"]["choice"] for i in ids}
    conf_c = {i: res_c[i]["answers"]["failure_class"]["confidence"] for i in ids}
    noul = {i: {q: a["noul"] for q, a in res_n[i]["answers"].items()} for i in ids}
    lab_n = {i: cases_n[i]["labels"] for i in ids}
    # 土法基準：這次所有失敗的測試在上一個 run 都還是綠的 → 這是新出現的紅 → 值得重跑一次
    base_new = {i: max(cases_c[i]["state"]["history"]["test_prior_consecutive"].values(), default=0) == 0
                for i in ids}

    print("樣本 n=%d（choice 與 noul 各一次呼叫，共 %d 次）" % (len(ids), 2 * len(ids)))
    print("類別分布：" + "  ".join("%s=%d" % (c, sum(truth[i] == c for i in ids)) for c in CLASSES if any(truth[i] == c for i in ids)))
    obs = [i for i in ids if meta[i]["rerun_basis"] == "observed"]
    print("其中 rerun 標籤是實際觀察到的（attempt 2 真的綠了）：%d 筆" % len(obs))

    for tag, path in (("choice", "results_choice.jsonl"), ("noul", "results_noul.jsonl")):
        rs = [json.loads(l) for l in open(f"{D}/{path}")]
        ms = sorted(r["ms"] for r in rs)
        tin = sum(r["usage"]["input_tokens"] for r in rs)
        tout = sum(r["usage"]["output_tokens"] for r in rs)
        print("\n[%s] 延遲 p50=%.0fms p95=%.0fms max=%.0fms；input %d tokens、output %d tokens；錯誤 %d 次"
              % (tag, ms[len(ms) // 2], ms[int(.95 * (len(ms) - 1))], ms[-1], tin, tout, sum(1 for r in rs if r["error"])))

    print("\n=== 1) choice：混淆矩陣（列＝人工標籤，欄＝Jev）===")
    used = [c for c in CLASSES if any(truth[i] == c for i in ids) or any(pred_c[i] == c for i in ids)]
    print(table([c for c in CLASSES if any(truth[i] == c for i in ids)], used,
                lambda t, p: sum(1 for i in ids if truth[i] == t and pred_c[i] == p)))
    print("\n每類 precision / recall：")
    for c in used:
        tp = sum(1 for i in ids if truth[i] == c and pred_c[i] == c)
        fp = sum(1 for i in ids if truth[i] != c and pred_c[i] == c)
        fn = sum(1 for i in ids if truth[i] == c and pred_c[i] != c)
        print("  %-20s P=%s R=%s  (tp=%d fp=%d fn=%d)" % (
            c, "%.2f" % (tp / (tp + fp)) if tp + fp else "—", "%.2f" % (tp / (tp + fn)) if tp + fn else "—", tp, fp, fn))
    acc = sum(truth[i] == pred_c[i] for i in ids) / len(ids)
    major = max(sum(truth[i] == c for i in ids) for c in CLASSES) / len(ids)
    print("  整體 acc=%.2f（全猜最大類的基準 %.2f）" % (acc, major))
    print("  信心分層：")
    for lo, hi in ((0, .5), (.5, .8), (.8, 1.01)):
        b = [i for i in ids if lo <= conf_c[i] < hi]
        if b:
            print("    conf∈[%.1f,%.1f) n=%d acc=%.2f" % (lo, min(hi, 1), len(b), sum(truth[i] == pred_c[i] for i in b) / len(b)))

    print("\n=== 2) noul 三題：acc@0.5 與 Brier ===")
    for q in ("rerun_would_pass", "machine_specific", "introduced_by_this_commit"):
        pv = [(noul[i][q], bool(lab_n[i][q])) for i in ids if q in lab_n[i]]
        acc = sum((p >= .5) == y for p, y in pv) / len(pv)
        brier = sum((p - y) ** 2 for p, y in pv) / len(pv)
        base = max(sum(y for _, y in pv), len(pv) - sum(y for _, y in pv)) / len(pv)
        print("  %-26s n=%2d acc@0.5=%.2f（全猜多數 %.2f） brier=%.3f  p 的範圍 %.2f–%.2f"
              % (q, len(pv), acc, base, brier, min(p for p, _ in pv), max(p for p, _ in pv)))

    print("\n=== 3) 「重跑一次」政策：把 rerun 建議當成正類 ===")
    print("  eligible = 判給重跑的那一批；precision = 其中真的重跑會綠的比例（#266 的升級門檻）")
    truth_rerun = {i: bool(lab_n[i]["rerun_would_pass"]) for i in ids}

    def policy(name, pick):
        sel = [i for i in ids if pick(i)]
        tp = sum(truth_rerun[i] for i in sel)
        fp = len(sel) - tp
        tot = sum(truth_rerun.values())
        bad = [i for i in sel if not truth_rerun[i]]
        print("  %-42s eligible=%2d  precision=%s  recall=%.2f  誤放行=%s"
              % (name, len(sel), "%.2f" % (tp / len(sel)) if sel else "—", tp / tot,
                 ",".join(meta[i]["sha"] for i in bad) or "無"))

    policy("choice 說 flaky_timing", lambda i: pred_c[i] == "flaky_timing")
    policy("choice 說 flaky_timing 且 conf≥0.5", lambda i: pred_c[i] == "flaky_timing" and conf_c[i] >= .5)
    for th in (.4, .45, .5, .6):
        policy("noul rerun_would_pass ≥ %.2f" % th, lambda i, th=th: noul[i]["rerun_would_pass"] >= th)
    policy("土法：所有紅測試上一個 run 還是綠的", lambda i: base_new[i])
    policy("土法 且 noul ≥ 0.4", lambda i: base_new[i] and noul[i]["rerun_would_pass"] >= .4)
    policy("土法 且 noul ≥ 0.5", lambda i: base_new[i] and noul[i]["rerun_would_pass"] >= .5)

    print("\n=== 4) 每一題逐筆（真實 / choice / noul rerun 機率 / 土法）===")
    print("%-18s %-19s %-19s %-6s %-5s %-6s %s" % ("id", "label", "choice", "conf", "noul", "土法", "備註"))
    for i in ids:
        mark = "" if (noul[i]["rerun_would_pass"] >= .5) == truth_rerun[i] else "  ← rerun 判斷錯"
        print("%-18s %-19s %-19s %.2f   %.2f  %-6s%s%s" % (
            i, truth[i], pred_c[i], conf_c[i], noul[i]["rerun_would_pass"],
            "新" if base_new[i] else "舊", "  [實測重跑]" if meta[i]["rerun_basis"] == "observed" else "", mark))

    print("\n=== 5) 只看 5 筆『真的有人重跑而且綠了』的 ===")
    for i in obs:
        print("  %-18s noul=%.2f choice=%s(%.2f)" % (i, noul[i]["rerun_would_pass"], pred_c[i], conf_c[i]))
    print("  這 5 筆 noul≥0.5 命中 %d/5；choice 說 flaky_timing 命中 %d/5"
          % (sum(noul[i]["rerun_would_pass"] >= .5 for i in obs), sum(pred_c[i] == "flaky_timing" for i in obs)))


if __name__ == "__main__":
    main()
