#!/usr/bin/env python3
"""#267：把 results.jsonl 算成混淆矩陣、逐類 precision／recall，並和一組「規則表」對照。

用法：analyze.py cases.jsonl results.jsonl

規則表（`rules`）是我看過標籤之後才寫的，所以它是「規則能做到多好」的樂觀上界，
不是公平的留出對照；拿它來回答「Jev 有沒有贏過現成規則」時要記得這一點。
"""
import json
import sys

CLASSES = ["wait_quota", "ask_login", "restart_bot", "escalate_user", "no_action"]

LIMIT = ("reached your fable limit", "hit your usage limit", "ratelimit", "等額度回來", "usage-credits")
LOGIN = ("logged_in=some(false)", "not logged in on host")
RESTART_HEAD = ("restart for the claude update failed", "agent.wait did not settle")
HUMAN_HEAD = ("assignment could not be delivered", "排隊太久", "a scheduled ops script reported that it is stuck",
              "queued prompt put back on the queue", "the composer lost the head of the paste",
              "prompt stalled; turn failed", "could not read the pane before typing")
HUMAN_RECORD = ("intent_failed", "assignment_undeliverable", "ops_alert")


def rules(case):
    """AGM 現在就有的欄位寫得出來的判斷：關鍵字＋事件種類，沒有模型。"""
    st = case["state"]
    ev = st["evidence"]
    text = json.dumps(ev, ensure_ascii=False).lower()
    trig = (ev.get("trigger") or "").lower()
    head = case["meta"]["trigger_head"].lower()
    if any(k in trig for k in LOGIN):
        return "ask_login"
    if any(k in trig for k in LIMIT) and "is history" not in trig and "reset available" not in trig:
        # 畫面上的字是原始碼／diff 時不算撞限
        if "error: " in trig and ('",' in trig or " + " in trig or " - " in trig):
            return "no_action"
        return "wait_quota"
    if any(head.startswith(h) for h in RESTART_HEAD):
        return "restart_bot"
    if "bot_restart_failed" in text or "bot has no active run" in text:
        return "restart_bot"
    if any(head.startswith(h) for h in HUMAN_HEAD) or any(k in head for k in HUMAN_RECORD):
        return "escalate_user"
    return "no_action"


def score(name, pred, gold, ids):
    hit = sum(p == g for p, g in zip(pred, gold))
    print("\n== %s  acc=%.2f (%d/%d)" % (name, hit / len(gold), hit, len(gold)))
    print("   真\\預測  " + "".join("%-14s" % c[:13] for c in CLASSES))
    for g in CLASSES:
        row = [sum(1 for p, y in zip(pred, gold) if y == g and p == c) for c in CLASSES]
        print("   %-9s" % g + "".join("%-14d" % v for v in row))
    for c in CLASSES:
        tp = sum(1 for p, y in zip(pred, gold) if p == c and y == c)
        fp = sum(1 for p, y in zip(pred, gold) if p == c and y != c)
        fn = sum(1 for p, y in zip(pred, gold) if p != c and y == c)
        prec = tp / (tp + fp) if tp + fp else float("nan")
        rec = tp / (tp + fn) if tp + fn else float("nan")
        print("   %-14s precision=%.2f (%d/%d)  recall=%.2f (%d/%d)" % (c, prec, tp, tp + fp, rec, tp, tp + fn))
    bad = [(i, p, y) for i, p, y in zip(ids, pred, gold) if p == "restart_bot" and y != "restart_bot"]
    print("   誤勸重啟 %d 筆：%s" % (len(bad), ", ".join("%s(真=%s)" % (i, y) for i, _, y in bad)))


def main():
    cases = {json.loads(l)["id"]: json.loads(l) for l in open(sys.argv[1])}
    rows = [json.loads(l) for l in open(sys.argv[2]) if l.strip()]
    rows = [r for r in rows if not r["error"]]
    ids = [r["id"] for r in rows]
    gold = [r["labels"]["next_action"] for r in rows]
    jev = [r["answers"]["next_action"]["choice"] for r in rows]
    rule = [rules(cases[i]) for i in ids]
    maj = ["no_action"] * len(gold)
    score("Jev next_action", jev, gold, ids)
    score("規則表 next_action", rule, gold, ids)
    print("\n== 只猜最大類（no_action） acc=%.2f" % (sum(p == g for p, g in zip(maj, gold)) / len(gold)))

    print("\n== 規則表和 Jev 一起用（規則有話說就聽規則，剩下的問 Jev）")
    hybrid = [r if r != "no_action" else j for r, j in zip(rule, jev)]
    print("   acc=%.2f" % (sum(p == g for p, g in zip(hybrid, gold)) / len(gold)))

    print("\n== 兩邊都答對／都答錯／各對一半")
    both = sum(1 for j, r, g in zip(jev, rule, gold) if j == g and r == g)
    only_j = sum(1 for j, r, g in zip(jev, rule, gold) if j == g and r != g)
    only_r = sum(1 for j, r, g in zip(jev, rule, gold) if j != g and r == g)
    print("   都對=%d  只有 Jev 對=%d  只有規則對=%d  都錯=%d" % (both, only_j, only_r, len(gold) - both - only_j - only_r))
    print("   只有 Jev 對的：%s" % ", ".join(i for i, j, r, g in zip(ids, jev, rule, gold) if j == g and r != g))

    print("\n== noul 當「要不要上升到人」的門檻（needs_human）")
    for th in (0.5, 0.6, 0.7, 0.8):
        p = [r["answers"]["needs_human"]["noul"] >= th for r in rows]
        y = [r["labels"]["needs_human"] for r in rows]
        tp = sum(1 for a, b in zip(p, y) if a and b)
        fp = sum(1 for a, b in zip(p, y) if a and not b)
        fn = sum(1 for a, b in zip(p, y) if not a and b)
        print("   th=%.1f precision=%.2f recall=%.2f (tp=%d fp=%d fn=%d)"
              % (th, tp / (tp + fp) if tp + fp else float("nan"),
                 tp / (tp + fn) if tp + fn else float("nan"), tp, fp, fn))


if __name__ == "__main__":
    main()
