#!/usr/bin/env python3
"""#263 交辦回報證據旗標：把 40 則真實子 agent 完成回報做成 JSONL 題目。

  python3 build_cases.py > cases_zh.jsonl          # 讀正式 DB（唯讀），要在這台機器上
  python3 build_cases.py --from cases_zh.jsonl \
      --translations translations_en.json > cases_en.jsonl

人工標籤在 labels.json，先標完才呼叫 API。第三方專案／識別名一律換成代號才出門，
規則見 ALIASES；掃過沒有金鑰，只有「Bearer」這個字出現在散文裡。
"""
import json, os, re, sqlite3, sys

DB = os.path.expanduser("~/.config/agents-manager/agents-manager.sqlite3")
# 長的先換，免得短的先吃掉前綴。換的是別的專案／客戶側的名字，不是本 repo 自己的 bot。
ALIASES = [
    ("pt-hub-aawpak-ops", "beta-hub-ops"), ("pt-hub", "beta-hub"), ("pt2023", "beta2023"),
    ("witsper-ops", "acme-ops"), ("wits-c1-op-xh", "acme-c1-op-xh"), ("wits-ops", "acme-ops"),
    ("console-fetures", "eta-console"), ("hermes-agents", "theta-agents"),
    ("tosync-fe", "zeta-fe"), ("tosync-be", "zeta-be"),
    ("insurer2", "gamma2"), ("onvifdev", "deltadev"), ("rtsp", "epsilon"),
    ("pdfcheck", "iota-check"), ("R-廠代碼-移機", "R-proj-move"), ("廠代碼", "proj-code"),
]
# 出門前擋一次；這批掃過是 0 命中，留著是怕以後換樣本。
SECRET = re.compile(r"(gho_|ghp_|github_pat_|sk-[A-Za-z0-9]|xox[baprs]-|AKIA[0-9A-Z]{16}|-----BEGIN)")

QUESTIONS = {
    "claims_complete": {
        "type": "noul",
        "instructions": {
            "question": "Does `report` claim the assigned work is finished?",
            "focus": "Judge what the report asserts, not whether the assertion is true. An acknowledgement, a progress update, a question, or a report that says it stopped or is waiting is not a claim of completion.",
        },
        "criteria": {
            "true": "The report states the assigned work, or its main deliverable, is done, fixed, pushed or shipped",
            "false": "The report only acknowledges, reports progress, asks something, or says the main deliverable is still not done",
        },
    },
    "claims_verified": {
        "type": "noul",
        "instructions": {
            "question": "Does `report` claim that tests, a build, a type check or CI passed?",
            "focus": "A check that is described as still running, skipped or unconfirmed is not a claim that it passed.",
        },
        "criteria": {
            "true": "The report asserts at least one test run, build, lint/type check or CI run passed",
            "false": "No such claim, or the report says the check is still running, was skipped, or is unconfirmed",
        },
    },
    "contains_concrete_evidence": {
        "type": "noul",
        "instructions": {
            "question": "Does `report` include checkable concrete evidence for what it says it did?",
            "focus": "Concrete evidence is an identifier or a measurement a reader could look up or compare: a commit sha, a CI run id, a process id, a pass/fail count, a backup file name with a size, a before/after number. Prose alone, a bare file path, or a plan with no numbers is not evidence.",
        },
        "criteria": {
            "true": "At least one such identifier or measurement appears and backs something the report says it did",
            "false": "The report is prose only, with no identifier or measurement",
        },
    },
    "asks_parent_action": {
        "type": "noul",
        "instructions": {
            "question": "Does `report` need its recipient — the dispatcher or the user — to do something or decide something?",
            "focus": "Count an explicit request, an approval or deployment that is waiting on the recipient, a question left for them, or a remaining action handed to them. Do not count the reporter describing what it will do itself or what it told its own sub-agents to do. Do not count the bare line that this round did no release build and no restart.",
        },
        "criteria": {
            "true": "The report needs the recipient to act, approve, decide or answer",
            "false": "The report is informational; nothing is required from the recipient",
        },
    },
    "admits_unfinished": {
        "type": "noul",
        "instructions": {
            "question": "Does `report` name at least one loose end — something still not done, not verified, or blocked?",
            "focus": "Count anything the report itself names as outstanding: not deployed yet, could not verify, blocked on a quota or a lock, a defect left to someone else, a part explicitly not touched. Do not count the routine scope line that this round did no release build and no restart, and do not count the reporter merely explaining a design choice it made.",
        },
        "criteria": {
            "true": "The report names at least one outstanding or unverified item",
            "false": "The report presents the work as finished and checked, with nothing outstanding named",
        },
    },
}
QIDS = list(QUESTIONS)


def redact(text):
    for a, b in ALIASES:
        text = text.replace(a, b)
    if SECRET.search(text):
        sys.exit("看起來有金鑰，不送出去")
    return text


def cjk_ratio(text):
    letters = [c for c in text if c.isalpha()]
    return round(sum(c >= "　" for c in letters) / max(1, len(letters)), 3)


def emit(case_id, text, labels, meta):
    meta = dict(meta, chars=len(text), cjk_ratio=cjk_ratio(text))
    return {"id": case_id, "state": {"report": text}, "questions": QUESTIONS,
            "labels": labels, "meta": meta}


def main():
    lab = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "labels.json")))
    rows = {}
    if "--from" in sys.argv:
        src = sys.argv[sys.argv.index("--from") + 1]
        tr = json.load(open(sys.argv[sys.argv.index("--translations") + 1]))
        base = {json.loads(l)["id"]: json.loads(l) for l in open(src) if l.strip()}
        for case_id in lab["translated"]:
            src_case = base[case_id]
            print(json.dumps(emit(case_id + "-en", tr[case_id], src_case["labels"],
                                  dict(src_case["meta"], arm="en", pair=case_id)), ensure_ascii=False))
        return
    db = sqlite3.connect("file:%s?mode=ro" % DB, uri=True)
    ids = [r[1] for r in lab["cases"]]
    q = ",".join("?" * len(ids))
    for aid, bot, dec, result in db.execute(
            "select a.id, coalesce(b.name,'?'), coalesce(a.review_decision,''), a.result "
            "from supervisor_assignments a left join bots b on b.id=a.target_bot_id "
            "where a.id in (%s)" % q, ids):
        rows[aid] = (bot, dec, result)
    for case_id, aid, *vals in lab["cases"]:
        bot, dec, result = rows[aid]
        text = redact(result.strip())
        labels = dict(zip(QIDS, [bool(v) for v in vals]))
        meta = {"arm": "zh", "group": case_id[0], "agm_review": dec or "none"}
        print(json.dumps(emit(case_id, text, labels, meta), ensure_ascii=False))


if __name__ == "__main__":
    main()
