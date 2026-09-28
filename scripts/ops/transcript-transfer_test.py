#!/usr/bin/env python3
"""scripts/ops/transcript-transfer 的隔離測試（issue #717）：假的來源 $HOME、假的目標目錄、手做的 bundle，
不碰真的 ~/.claude／~/.codex／~/.grok，也不 ssh。"""

import gzip
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
TOOL = os.path.join(HERE, "transcript-transfer")
SRC_PROJ = "/Users/m4p/project/am"
DST_PROJ = "/home/ubuntu/project/am"
WT = SRC_PROJ + "/.claude/worktrees/am-x1"


def claude_key(p):
    return "".join(c if c.isalnum() else "-" for c in p)


class Env:
    def __init__(self, tmp):
        self.src = os.path.join(tmp, "src-home")
        self.dst = os.path.join(tmp, "dst-home")
        self.tmp = tmp
        os.makedirs(self.src)
        os.makedirs(self.dst)

    def put(self, rel, text):
        p = os.path.join(self.src, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w", encoding="utf-8") as f:
            f.write(text)
        return p

    def bundle(self, bots, runs, name="in.json.gz"):
        cols = lambda rows: sorted({k for r in rows for k in r})
        b = {"format": "agm-project-transfer/1", "source_host": "local", "tables": {
            "bots": {"columns": cols(bots), "rows": bots}, "runs": {"columns": cols(runs), "rows": runs}}}
        p = os.path.join(self.tmp, name)
        with gzip.open(p, "wb") as f:
            f.write(json.dumps(b).encode())
        return p

    def run(self, bundle, *extra, maps=(f"{SRC_PROJ}={DST_PROJ}",)):
        out = os.path.join(self.tmp, "out.json.gz")
        argv = [sys.executable, TOOL, "--bundle", bundle, "--out", out, "--target-dir", self.dst,
                "--source-home", self.src] + [a for m in maps for a in ("--map", m)] + list(extra)
        r = subprocess.run(argv, capture_output=True, text=True)
        report = json.loads(r.stdout) if r.stdout.strip().startswith("{") else None
        return r, report, out

    def dst_read(self, rel):
        with open(os.path.join(self.dst, rel), encoding="utf-8") as f:
            return f.read()


def claude_session(env, cwd, sid, cfg=".claude"):
    lines = [
        {"type": "user", "cwd": cwd, "sessionId": sid, "message": {"content": f"read {cwd}/note.txt"}},
        {"type": "assistant", "cwd": cwd, "message": {"content": [{"type": "tool_use", "input": {"file_path": f"{cwd}/note.txt"}}]}},
        {"type": "user", "cwd": cwd, "toolUseResult": {"file": {"filePath": f"{cwd}/note.txt"}},
         "note": f"sibling {SRC_PROJ}-other/x stays; config {os.path.join('/Users/m4p', cfg)}/projects/{claude_key(cwd)}/{sid}.jsonl"},
    ]
    return env.put(f"{cfg}/projects/{claude_key(cwd)}/{sid}.jsonl", "".join(json.dumps(l) + "\n" for l in lines))


def bot(bid, kind):
    return {"id": bid, "kind": kind}


def run(rid, bid, sid, path):
    return {"id": rid, "bot_id": bid, "native_session_id": sid, "transcript_path": path}


class TranscriptTransfer(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="agm-717-")
        self.env = Env(self._tmp.name)
        # 內容裡的 `/Users/m4p` 要換成目標 $HOME；測試的來源 $HOME 是暫存目錄，所以另外把 /Users/m4p 也對到目標。
        self.maps = (f"{SRC_PROJ}={DST_PROJ}", f"/Users/m4p={self.env.dst}")

    def tearDown(self):
        self._tmp.cleanup()

    def test_claude_moves_under_the_target_cwd_key_and_rewrites_paths(self):
        e = self.env
        src = claude_session(e, SRC_PROJ, "s-c1")
        os.makedirs(os.path.join(os.path.dirname(src), "s-c1", "subagents"))
        e.put(f".claude/projects/{claude_key(SRC_PROJ)}/s-c1/subagents/a.jsonl", json.dumps({"cwd": SRC_PROJ}) + "\n")
        with open(src) as f:
            before = f.read()
        b = e.bundle([bot("b1", "claude")], [run("r1", "b1", "s-c1", src), run("r2", "b1", "s-c1", src)])
        r, rep, out = e.run(b, maps=self.maps)
        self.assertEqual(r.returncode, 0, r.stderr + r.stdout)
        rel = f".claude/projects/{claude_key(DST_PROJ)}/s-c1.jsonl"
        text = e.dst_read(rel)
        self.assertNotIn(SRC_PROJ + "/", text.replace(SRC_PROJ + "-other", ""), "專案路徑全換掉")
        self.assertIn(f'"cwd": "{DST_PROJ}"', text)
        self.assertIn(f"{DST_PROJ}/note.txt", text)
        self.assertNotIn(f"{DST_PROJ}-other", text, "只換完整的路徑段：兄弟目錄不吃專案那條 --map")
        self.assertIn(f"{e.dst}/project/am-other/x", text, "兄弟目錄落到較短的 $HOME 那條")
        self.assertIn(f"/projects/{claude_key(DST_PROJ)}/s-c1.jsonl", text, "內容裡的 claude 目錄名也換")
        self.assertIn(DST_PROJ, e.dst_read(f".claude/projects/{claude_key(DST_PROJ)}/s-c1/subagents/a.jsonl"), "同名附屬目錄一起搬")
        with open(src) as f:
            self.assertEqual(f.read(), before, "來源檔一個字都不動")
        with gzip.open(out) as f:
            runs = json.loads(f.read())["tables"]["runs"]["rows"]
        self.assertEqual({x["transcript_path"] for x in runs}, {os.path.join(e.dst, rel)}, "兩個 run 都指到目標上的檔")
        self.assertEqual(len(rep["sessions"]), 1, "同一段只搬一次")
        self.assertEqual(oct(os.stat(os.path.join(e.dst, rel)).st_mode & 0o777), "0o600")

    def test_a_worktree_cwd_follows_the_project_map(self):
        e = self.env
        src = claude_session(e, WT, "s-wt", cfg=".claude-cc1")
        b = e.bundle([bot("b1", "claude")], [run("r1", "b1", "s-wt", src)])
        r, rep, _ = e.run(b, maps=self.maps)
        self.assertEqual(r.returncode, 0, r.stderr)
        new_wt = DST_PROJ + "/.claude/worktrees/am-x1"
        self.assertIn(f'"cwd": "{new_wt}"', e.dst_read(f".claude-cc1/projects/{claude_key(new_wt)}/s-wt.jsonl"),
                      "身分的 config 目錄（.claude-cc1）照原樣、目錄名由新的 worktree 路徑算")

    def test_codex_keeps_its_dated_path_and_rewrites_content(self):
        e = self.env
        rel = ".codex/sessions/2026/09/28/rollout-2026-09-28T15-50-19-s-x1.jsonl"
        src = e.put(rel, json.dumps({"type": "session_meta", "payload": {"cwd": SRC_PROJ}}) + "\n")
        b = e.bundle([bot("b1", "codex")], [run("r1", "b1", "s-x1", None)])
        r, rep, out = e.run(b, maps=self.maps)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn(DST_PROJ, e.dst_read(rel))
        self.assertEqual(rep["sessions"][0]["from"], src, "DB 沒記路徑：照 kind 找到")
        with gzip.open(out) as f:
            self.assertEqual(json.loads(f.read())["tables"]["runs"]["rows"][0]["transcript_path"], os.path.join(e.dst, rel))

    def test_grok_moves_the_whole_session_dir_under_the_new_encoded_cwd(self):
        e = self.env
        import urllib.parse
        key = urllib.parse.quote(SRC_PROJ, safe="")
        base = f".grok/sessions/{key}/s-g1"
        upd = e.put(f"{base}/updates.jsonl", json.dumps({"cwd": SRC_PROJ}) + "\n")
        e.put(f"{base}/chat_history.jsonl", json.dumps({"text": f"see {SRC_PROJ}/a"}) + "\n")
        e.put(f"{base}/system_prompt.txt", f"cwd is {SRC_PROJ}\n")
        e.put(f"{base}/summary.json.lock", "")
        b = e.bundle([bot("b1", "grok")], [run("r1", "b1", "s-g1", upd)])
        r, rep, _ = e.run(b, maps=self.maps)
        self.assertEqual(r.returncode, 0, r.stderr)
        nb = f".grok/sessions/{urllib.parse.quote(DST_PROJ, safe='')}/s-g1"
        self.assertIn(DST_PROJ, e.dst_read(f"{nb}/chat_history.jsonl"))
        self.assertIn(DST_PROJ, e.dst_read(f"{nb}/system_prompt.txt"))
        self.assertFalse(os.path.exists(os.path.join(e.dst, nb, "summary.json.lock")), "lock 檔不搬")
        self.assertEqual(rep["sessions"][0]["to"], os.path.join(e.dst, nb, "updates.jsonl"))

    def test_rerun_is_idempotent_and_a_changed_target_is_not_overwritten(self):
        e = self.env
        src = claude_session(e, SRC_PROJ, "s-c2")
        b = e.bundle([bot("b1", "claude")], [run("r1", "b1", "s-c2", src)])
        self.assertEqual(e.run(b, maps=self.maps)[0].returncode, 0)
        r, rep, _ = e.run(b, maps=self.maps)
        self.assertEqual((r.returncode, rep["files_written"], rep["files_same"]), (0, 0, 1), "重跑：內容一樣就跳過")
        rel = f".claude/projects/{claude_key(DST_PROJ)}/s-c2.jsonl"
        with open(os.path.join(e.dst, rel), "a") as f:
            f.write('{"type":"user","message":"目標接著寫了一句"}\n')
        r, rep, _ = e.run(b, maps=self.maps)
        self.assertEqual(r.returncode, 2)
        self.assertEqual(rep["conflicts"], [rel])
        self.assertIn("目標接著寫了一句", e.dst_read(rel), "不蓋掉目標已經接著寫的對話")
        r, rep, _ = e.run(b, "--overwrite", maps=self.maps)
        self.assertEqual(r.returncode, 0)
        self.assertNotIn("目標接著寫了一句", e.dst_read(rel), "--overwrite 才蓋")

    def test_only_transcript_locations_are_ever_copied(self):
        e = self.env
        e.put(".codex/auth.json", '{"token":"SECRET"}')
        bad = os.path.join(e.src, ".codex/auth.json")
        cred = e.put(".claude/projects/k/.credentials.json", "SECRET")
        b = e.bundle([bot("b1", "codex"), bot("b2", "claude")],
                     [run("r1", "b1", "s-auth", bad), run("r2", "b2", "s-cred", cred)])
        r, rep, _ = e.run(b, maps=self.maps)
        self.assertEqual(r.returncode, 2)
        self.assertEqual(len(rep["refused"]) + len(rep["missing"]), 2, rep)
        for d, _, names in os.walk(e.dst):
            self.assertEqual(names, [], "目標一個檔都沒有")

    def test_an_ambiguous_or_missing_session_is_reported_not_guessed(self):
        e = self.env
        claude_session(e, SRC_PROJ, "s-dup")
        claude_session(e, SRC_PROJ, "s-dup", cfg=".claude-cc2")
        b = e.bundle([bot("b1", "claude")], [run("r1", "b1", "s-dup", "/gone/s-dup.jsonl"), run("r2", "b1", "s-none", None)])
        r, rep, out = e.run(b, maps=self.maps)
        self.assertEqual(r.returncode, 2)
        self.assertEqual({m["session"]: len(m["candidates"]) for m in rep["missing"]}, {"s-dup": 2, "s-none": 0})
        with gzip.open(out) as f:
            runs = {x["id"]: x["transcript_path"] for x in json.loads(f.read())["tables"]["runs"]["rows"]}
        self.assertEqual(runs["r1"], "/gone/s-dup.jsonl", "找不到的不亂改")

    def test_the_recorded_path_picks_the_identity_when_the_id_is_in_two_config_dirs(self):
        e = self.env
        claude_session(e, SRC_PROJ, "s-two")
        cc2 = claude_session(e, SRC_PROJ, "s-two", cfg=".claude-cc2")
        b = e.bundle([bot("b1", "claude")], [run("r1", "b1", "s-two", cc2)])
        r, rep, _ = e.run(b, maps=self.maps)
        self.assertEqual(r.returncode, 0, r.stdout)
        self.assertEqual(rep["sessions"][0]["from"], cc2, "DB 記的那份（換過身分只有它知道是哪個 config 目錄）")
        self.assertTrue(rep["sessions"][0]["to"].startswith(os.path.join(e.dst, ".claude-cc2/")))

    def test_a_cwd_outside_every_map_falls_back_to_the_target_home(self):
        e = self.env
        cwd = os.path.join(e.src, "scratch/p")
        src = claude_session(e, cwd, "s-home")
        b = e.bundle([bot("b1", "claude")], [run("r1", "b1", "s-home", src)])
        r, rep, _ = e.run(b, maps=(f"{SRC_PROJ}={DST_PROJ}",))
        self.assertEqual(r.returncode, 0, r.stdout)
        new = os.path.join(e.dst, "scratch/p")
        self.assertIn(f'"cwd": "{new}"', e.dst_read(f".claude/projects/{claude_key(new)}/s-home.jsonl"))

    def test_dry_run_writes_nothing(self):
        e = self.env
        src = claude_session(e, SRC_PROJ, "s-c3")
        b = e.bundle([bot("b1", "claude")], [run("r1", "b1", "s-c3", src)])
        r, rep, out = e.run(b, "--dry-run", maps=self.maps)
        self.assertEqual((r.returncode, rep["files_written"]), (0, 1))
        self.assertFalse(os.path.exists(out))
        for d, _, names in os.walk(e.dst):
            self.assertEqual(names, [])

    def test_bad_maps_are_refused(self):
        e = self.env
        b = e.bundle([bot("b1", "claude")], [])
        r, _, _ = e.run(b, maps=("relative=/x",))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("--map", r.stderr)


if __name__ == "__main__":
    unittest.main()
