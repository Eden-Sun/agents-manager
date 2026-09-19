#!/usr/bin/env python3
"""A：release-triage 逐條預篩的題目。用法：build_cases_a.py <ledger.json> <claude CHANGELOG.md> > cases_a.jsonl
標籤是 f1（Claude Fable 5.1）在呼叫 Jev 之前盲標的，不是 AGM 的 verdict——帳本當時沒有任何已判的版本。"""
import json, re, sys
ledger = json.load(open(sys.argv[1]))
md = open(sys.argv[2]).read()
CODEX = {"abc15d424e": "guard", "7361458176": "none", "9f7b1914a9": "guard", "5ba052f348": "none", "c3343d18bf": "none",
         "ea082a70d9": "adopt", "f1b2d3080e": "none", "14e39c6281": "upgrade-arg", "1a6132cf70": "upgrade-arg",
         "cd9195b5b9": "none", "1ceba75e1c": "none", "a98220da05": "guard", "878afc456e": "none", "9c8062c825": "none"}
CLAUDE = {1: "guard", 4: "upgrade-arg", 7: "upgrade-arg", 11: "upgrade-arg", 12: "upgrade-arg", 13: "upgrade-arg", 29: "guard", 32: "guard"}
DEPENDS = ["SessionStart / Stop / StopFailure / Notification hooks", "text drawn on the terminal screen (status row, banners, dialogs, menus, composer)",
           "--resume and saved session transcripts", "statusline JSON", "usage-limit / rate-limit notices and /usage output",
           "login state and switching between accounts", "/model and /effort pickers driven by key presses", "project instruction files (CLAUDE.md)"]
Q = {
 "touches_agm": {"type": "noul", "instructions": "Does `entry` change the behaviour of anything listed in `agm_depends_on`?"},
 "verdict": {"type": "choice",
   "instructions": "`entry` is one changelog line of a coding-agent CLI. A supervisor program drives that CLI unattended through the things in `agm_depends_on`. How should the supervisor's maintainers react?",
   "criteria": {
     "guard": {"what": "What the supervisor reads or relies on now looks or behaves differently, so its integration may silently misread or break and needs checking", "not_for": "bug fixes that only make the CLI more correct without changing what the supervisor sees"},
     "adopt": {"what": "A new capability the supervisor could start using to do its job better"},
     "upgrade-arg": {"what": "A fix for a failure the supervisor could actually hit; nothing to change, just a reason to upgrade"},
     "none": {"what": "Unrelated to the supervisor: IDE extensions, plugins, cloud providers, Windows, cosmetics, features it does not use"}}}}
def case(i, cli, ver, text, label):
    return {"id": i, "state": {"cli": cli, "version": ver, "entry": text, "agm_depends_on": DEPENDS}, "questions": Q,
            "labels": {"verdict": label, "touches_agm": label != "none"}, "meta": {"cli": cli, "relevant": label in ("guard", "adopt")}}
for r in ledger["rows"]:
    for e in r["entries"]:
        print(json.dumps(case("codex-" + e["id"], "codex", r["version"], e["text"], CODEX[e["id"]]), ensure_ascii=False))
sec = md.split("## 2.1.277", 1)[1].split("\n## ", 1)[0]
bullets = [l[2:] for l in sec.splitlines() if l.startswith("- ")][::2][:36]
for n, b in enumerate(bullets, 1):
    print(json.dumps(case("claude-%02d" % n, "claude", "2.1.277", b, CLAUDE.get(n, "none")), ensure_ascii=False))
