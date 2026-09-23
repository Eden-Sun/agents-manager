#!/usr/bin/env python3
"""C：撞題／重複票提示的題目（#264）。

用法：
  gh issue list -R Eden-Sun/agents-manager --state all --limit 500 \
      --json number,title,body > issues.json
  build_cases_c.py issues.json > cases_c.jsonl      # 順便寫出 issues_used.json

每列一對（新交辦 vs 既有 issue／進行中交辦）。標籤 PAIRS 寫死在這個檔裡，
**先於任何 API 呼叫**，來源是 repo 的 issue 歷史本身（票面互相聲明「同 #N 類」
「#N 後續」「同一個根因」的家族，與同子系統但各自獨立的票）。
不送 key、不接 daemon；state 只含 issue 的標題、前 450 字與從票面抓到的路徑。
"""
import json
import re
import sys

SUMMARY_CHARS = 450

# (候選, 既有, 標籤 0-3, 既有是不是「進行中的交辦」, 為什麼這樣標)
# 3＝實質同一件事；2＝同一個根因／工作重疊；1＝同子系統但各自獨立；0＝無關
PAIRS = [
    # --- G1 撞限偵測「整行含有」（候選 #237）---
    (237, 227, 2, False, "同一個 screen.rs 的兩個 provider 函式，同一個『整行含有』根因；#237 票面自稱『跟 #227 的 grok 舊版一樣』"),
    (237, 222, 1, False, "同樣是 grok／codex 撞限偵測區，但 #222 是『選單認不出來』的漏判，與誤判方向相反"),
    (237, 236, 1, False, "同樣是額度判定，但 #236 的根因是重置時間已過期就被丟掉，與畫面比對無關"),
    (237, 321, 1, False, "同一個 screen.rs 的解析程式，但 #321 是狀態列 parse 回 None，不是撞限誤判"),
    (237, 314, 0, False, "一個是額度偵測、一個是網頁標題列的排版"),
    # --- G2 讀不到主機退回 local（候選 #208）---
    (208, 198, 2, False, "#208 票面自稱『#198 同類』：authority 讀不到就當本機"),
    (208, 243, 2, False, "#243 就是同一類剩下的 call site 盤點，票面點名 #198／#208"),
    (208, 146, 1, False, "同一個 stop.rs／stop_bot 流程，但 #146 是狀態寫不進去仍做破壞性動作"),
    (208, 199, 1, False, "同一個使用者停止流程，但 #199 是撤回排隊訊息不在同一個交易"),
    (208, 294, 0, False, "一個是 daemon 的主機判定、一個是網頁的目錄選擇器"),
    # --- G3 多步驟中途崩潰（候選 #354）---
    (354, 296, 2, True, "#355 設計提案點名 #248 #284 #296 #353 #354 是同一個根因族：先 commit 的那一步留半套"),
    (354, 284, 2, False, "同上；不同檔（delete_project vs 重啟流程），同一個崩潰視窗根因"),
    (354, 346, 1, False, "同樣是一鍵重啟，但 #346 是停機那一步少了『還是 idle 才准停』的許可"),
    (354, 231, 1, False, "同樣是重啟，但 #231 是網頁把 503／409 當失敗、側欄不刷新"),
    (354, 359, 0, False, "一個是重啟崩潰視窗、一個是群組輸入框的讀屏可及性"),
    # --- G4 多分頁 localStorage 整份覆寫（候選 #366）---
    (366, 364, 2, False, "#366 票面自稱『同型 #364』：整份寫回、只該套有增減的鍵"),
    (366, 369, 1, False, "同一批 localStorage 草稿問題，但 #369 是沒聽 storage 事件，修法與覆寫無關"),
    (366, 365, 1, False, "同樣是網頁狀態同步，但 #365 是 resync 進行中被丟掉"),
    (366, 278, 1, False, "同樣是側欄，但 #278 是搜尋請求晚到覆蓋結果"),
    (366, 148, 0, False, "一個是網頁多分頁儲存、一個是遠端編譯的租約 token"),
    # --- G5 丟回應後的重複外部動作（候選 #352，英文）---
    (352, 348, 2, False, "兩票都是『回應遺失後重試，外部動作做了第二次』，缺的是 request identity"),
    (352, 337, 1, False, "同樣是冪等鍵，但 #337 是同 id 換內容回舊結果，方向相反（少送而不是多做）"),
    (352, 396, 1, False, "同樣是 bot 生命週期，但 #396 是整批重啟不該動到 child"),
    (352, 301, 1, False, "同樣是 create/restore/delete 競態，但 #301 缺的是 per-bot 鎖"),
    (352, 345, 0, False, "一個是重複建 bot、一個是設定檔權限外洩"),
    # --- G6 先 commit 狀態再 best-effort 通知（候選 #251，英文）---
    (251, 310, 2, False, "同一個形狀：標記先落地、通知另外寫且被吞，之後不再補；不同檔（watchdog.rs vs controller.rs）"),
    (251, 319, 2, False, "同一個形狀（incidents.rs）；#319 票面自稱『同 #283 形狀』，與 #251 同族"),
    (251, 249, 1, False, "同一個 watchdog，但 #249 是讀不到存活狀態當成停掉"),
    (251, 300, 1, False, "同樣是 supervisor 開機路徑，但 #300 是讀不到就 return、整個 controller 不啟動"),
    (251, 303, 0, False, "一個是 supervisor 通知、一個是網頁 markdown 圖片重掛"),
    # --- G7 ops 排程腳本靜默停擺（候選 #312）---
    (312, 311, 2, False, "同一個缺陷在兩支 kick 腳本：連續失敗只寫 local log、沒有人被叫醒"),
    (312, 316, 2, False, "同上，daemon-update-kick 的多數出口；#316 是同一族"),
    (312, 308, 1, False, "同一支 claude-release-kick，但 #308 是 mkdir 鎖殘留，不是失敗沒人知道"),
    (312, 371, 1, False, "同樣是 kick 腳本，但 #371 是 zsh glob 沒命中就中止整輪"),
    (312, 280, 0, False, "一個是排程腳本、一個是網頁額度條的寬度量測"),
    # --- G8 額度讀數被舊快照蓋掉（候選 #404）---
    (404, 399, 2, False, "#404 票面自稱『#399 後續』：舊 statusline 讀數蓋過新窗"),
    (404, 238, 1, False, "同樣是額度記錯格，但 #238 的根因是 run 沒記啟動時的身分"),
    (404, 392, 1, False, "同樣是額度顯示，但 #392 是重啟後讀數只在記憶體、要快取"),
    (404, 225, 1, False, "同樣是額度格記錯，但 #225 是 grok 週限記到 codex 那一格"),
    (404, 374, 0, False, "一個是額度讀數、一個是 dev server 的 Fast Refresh"),
    # --- 額外：歷史上真的撞在一起／真的重複的票 ---
    (98, 76, 2, True, "史實：#76 的修正 Bot 發現 736495a8 幾乎同時落地同一個修法，另一顆 Bot 接的正是 #98"),
    (44, 1, 3, False, "同一個 group::messages ULID 分頁問題，#44 是『#1 的修正沒有進 main』"),
    (140, 101, 3, False, "#140 票面自稱『#101 重開漏網的兩處』：同一件事剩下的 call site"),
    (218, 217, 3, False, "兩票同一天開、標題幾乎一樣：claude 2.1.278 的 <pasted_content> 讓送達證據對不上"),
    (125, 68, 3, True, "#125 是 #68 TurnController 集中化的收尾，同一件工作的後半"),
    (287, 21, 2, False, "同一個根因（herdr server 起了不 wait 留 zombie）在不同呼叫點"),
    (288, 272, 1, False, "硬負例：兩票都是『外部呼叫沒有逾時』，但一個是本機 ps、一個是 herdr --skill，修法與檔案都不重疊"),
    (264, 262, 1, False, "硬負例：同一批 Jev 評估票，用詞高度重疊，但評估的是不同場景"),
]

PATH_RE = re.compile(r"[A-Za-z0-9_./-]*[A-Za-z0-9_]\.(?:rs|ts|tsx|sh|py|toml|md)\b")
FENCE_RE = re.compile(r"```.*?```", re.S)


def summarize(body):
    text = FENCE_RE.sub(" ", body or "")
    text = re.sub(r"[#*>`|]", "", text)
    text = re.sub(r"\s+", " ", text).strip()
    return text[:SUMMARY_CHARS]


def paths(body):
    """票面提到的原始檔。回傳 issue 自己寫的字串（給模型看）與 basename（給確定性比對用）。"""
    out = []
    for p in PATH_RE.findall(body or ""):
        if p not in out:
            out.append(p)
    return out[:8]


LEVELS = [
    "Unrelated: different subsystems or different kinds of work; a person reading both would not connect them.",
    "Same subsystem or same file, but a distinct problem with its own fix; both can be worked on independently.",
    "Likely the same underlying root cause, or work that overlaps enough that two agents would redo or conflict with each other; they should be linked or merged before a second one starts.",
    "Effectively the same task: one is a restatement, a leftover part, or the unfinished remainder of the other.",
]
Q = {
    "collision": {
        "type": "score",
        "instructions": {
            "question": "How much does `candidate`, a piece of work somebody is about to start, collide with `existing`, a work item that is already filed or already being worked on?",
            "focus": "Judge the underlying defect and the change each one needs, not how similar the wording is. Two items in the same file can be separate bugs; two items in different files can be one root cause. `touches` lists file paths each item names, which may be incomplete.",
        },
        "criteria": LEVELS,
    },
    "same_work": {
        "type": "noul",
        "instructions": {
            "question": "Before a second agent starts on `candidate`, should a maintainer link or merge it with `existing` rather than let the two run independently?",
            "focus": "Answer yes only when the two would fix the same underlying cause or would step on each other's change. Sharing a subsystem, a file or vocabulary is not enough.",
        },
        "criteria": {
            "true": "A maintainer would stop and connect the two items first",
            "false": "The two items can proceed as separate work",
        },
    },
}


def lang(*texts):
    joined = "".join(texts)
    return "zh" if sum(1 for c in joined if "一" <= c <= "鿿") > len(joined) * 0.15 else "en"


def main():
    src = {x["number"]: x for x in json.load(open(sys.argv[1]))}
    used = {}
    for cand, exist, label, in_flight, _why in PAIRS:
        for n in (cand, exist):
            if n not in used:
                i = src[n]
                used[n] = {"number": n, "title": i["title"], "summary": summarize(i["body"]), "touches": paths(i["body"])}
    json.dump(used, open("issues_used.json", "w"), ensure_ascii=False, indent=1, sort_keys=True)
    for cand, exist, label, in_flight, why in PAIRS:
        c, e = used[cand], used[exist]
        state = {
            "project": "agents-manager",
            "candidate": {"kind": "work somebody is about to start", "title": c["title"], "summary": c["summary"], "touches": c["touches"]},
            "existing": {
                "kind": "an assignment another agent is working on right now" if in_flight else "an issue that is already open",
                "ref": "#%d" % exist, "title": e["title"], "summary": e["summary"], "touches": e["touches"]},
        }
        base = lambda ps: {p.rsplit("/", 1)[-1] for p in ps}
        overlap = bool(base(c["touches"]) & base(e["touches"]))
        print(json.dumps({
            "id": "%d-vs-%d" % (cand, exist),
            "state": state,
            "questions": Q,
            "labels": {"collision": label, "same_work": label >= 2},
            "meta": {"candidate": cand, "existing": exist, "label": label, "positive": label >= 2,
                     "in_flight": in_flight, "path_overlap": overlap,
                     "lang": lang(c["title"], c["summary"], e["title"], e["summary"]),
                     "why": why},
        }, ensure_ascii=False))


if __name__ == "__main__":
    main()
