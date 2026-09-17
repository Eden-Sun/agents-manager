#!/usr/bin/env python3
"""OB: project-ID-isolated web GPT consultations, operated by one Sonnet worker."""
import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

from ob_store import OBError, Store, exclusive, project_id

HERE = Path(__file__).resolve().parent


def daemon_project(pid):
    from agm import Client
    state = Client("http://127.0.0.1:7788").get("/api/supervisor/state")
    if not pid:
        bot = next((b for b in state["bots"] if b["id"] == os.environ.get("AM_BOT_ID")), None)
        pid = bot["project_id"] if bot else None
    project_id(pid)
    project = next((p for p in state["projects"] if p["id"] == pid), None)
    if not project:
        raise OBError("project_not_found：不以目錄名猜專案")
    if project.get("host") not in (None, "local"):
        raise OBError("remote_project：請在提供此 project 的本機 OB worker 提交")
    return project


def kick(store):
    if not store.setting("operator"):
        return False
    # The process-level lock makes concurrent kicks harmless; no model call when busy.
    with open(store.root / "worker.log", "a") as log:
        subprocess.Popen([sys.executable, "-B", str(HERE / "ob.py"), "--data-dir", str(store.root), "work"],
                         cwd=store.root, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                         start_new_session=True, close_fds=True)
    return True


def recover(store, start=True):
    from ob_operator import recover_orphaned
    recovered = recover_orphaned(store)
    if start and recovered and store.claimable():
        kick(store)  # queued work behind the dead worker; recovered rows stay unknown and are never claimed
    return recovered


def resolve_unknown(store, ident, not_sent=False):
    from ob_operator import journal_for
    with exclusive(store.root / "browser.lock"), store.transaction():
        job = store.get(ident)
        if job["status"] != "unknown":
            raise OBError("request_not_unknown")
        if not not_sent:
            raise OBError("先檢查原對話；確定沒送出才可 --confirmed-not-sent")
        journal = journal_for(store, ident)
        if journal.exists():
            # Keep the original evidence rather than silently erase a possibly sent request.
            journal.rename(journal.with_name(journal.name + f".resolved-{time.time_ns()}"))
        store.db.execute("UPDATE requests SET status='pending',error='operator_confirmed_not_sent',updated_at=? WHERE id=?", (time.time(), ident))
    return store.get(ident)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-dir", default=os.environ.get("OB_DATA_DIR", "~/.config/agents-manager/ob"))
    commands = parser.add_subparsers(dest="command", required=True)
    ask = commands.add_parser("ask", help="提交；相同 project ID + request ID 冪等")
    ask.add_argument("--project-id", "-p")
    ask.add_argument("--request-id", required=True)
    ask.add_argument("--file", "-f")
    ask.add_argument("text", nargs="?")
    ask.add_argument("--wait", type=float, default=0, help="最多等待秒數；逾時不取消或重送")
    ask.add_argument("--no-start", action="store_true")
    status = commands.add_parser("status")
    status.add_argument("id", nargs="?")
    status.add_argument("--project-id")
    cfg = commands.add_parser("configure", help="只指定一個 Sonnet 訂閱帳號，不設定 fallback")
    cfg.add_argument("--claude-config-dir", required=True)
    cfg.add_argument("--claude-binary", default=shutil.which("claude"))
    cfg.add_argument("--ego-binary", default=shutil.which("ego-browser"))
    work = commands.add_parser("work", help="單一 worker；空佇列不消耗模型 token")
    work.add_argument("--once", action="store_true")
    commands.add_parser("recover", help="worker 已不在時把殘留 running 轉 unknown（不重送）；worker 存活則不動")
    commands.add_parser("stop", help="請 worker 做完當前請求後停止；不打斷已送出的網頁問題")
    retry = commands.add_parser("retry")
    retry.add_argument("id")
    collect = commands.add_parser("collect", help="只取回 unknown 請求的原回答，不送訊息")
    collect.add_argument("id")
    resolve = commands.add_parser("resolve", help="對帳確認沒送出後才解除 unknown")
    resolve.add_argument("id")
    resolve.add_argument("--confirmed-not-sent", action="store_true", required=True)
    rotate = commands.add_parser("rotate", help="下一題改開新的一串（舊的留著查得到）")
    rotate.add_argument("--project-id", required=True)
    link = commands.add_parser("link", help="明確綁定既有對話；不根據 label 自動猜測")
    link.add_argument("--project-id", required=True)
    which = link.add_mutually_exclusive_group(required=True)
    which.add_argument("--url")
    which.add_argument("--legacy-key", help="沿用舊 chatgpt-consult.json 的指定 entry（保留原檔）")
    mcp = commands.add_parser("_mcp", help="內部工具入口，請使用 ask")
    mcp.add_argument("id")
    mcp.add_argument("token", nargs="?")
    args = parser.parse_args(argv)
    os.umask(0o077)
    store = Store(args.data_dir)
    if args.command == "_mcp":
        from ob_operator import mcp_server
        mcp_server(store.root, args.id, args.token)
        return 0
    if args.command == "configure":
        config_dir = Path(args.claude_config_dir).expanduser().resolve()
        if not config_dir.is_dir():
            raise OBError("Claude 訂閱設定目錄不存在")
        for exe in (args.claude_binary, args.ego_binary):
            if not exe or not Path(exe).is_file() or not os.access(exe, os.X_OK):
                raise OBError("claude／ego-browser 必須是存在且可執行的絕對路徑")
        with exclusive(store.root / "worker.lock"):
            store.set_setting("operator", dict(model="sonnet", effort="low", claude_config_dir=str(config_dir),
                                              claude_binary=str(Path(args.claude_binary).absolute()),
                                              ego_binary=str(Path(args.ego_binary).absolute())))
        result = {"configured": True, "model": "sonnet", "effort": "low", "fallback": None}
    elif args.command == "ask":
        if bool(args.file) == bool(args.text):
            raise OBError("只提供文字或 --file 其中一個")
        if args.wait < 0 or args.wait > 3600:
            raise OBError("--wait 必須在 0..3600 秒")
        project = daemon_project(args.project_id)
        question = Path(args.file).read_text() if args.file else args.text
        result = store.submit(project["id"], project["label"], args.request_id, question, os.environ.get("AM_BOT_ID"))
        if result["status"] == "running" and recover(store, not args.no_start):
            result = store.get(result["id"])  # replay after a worker crash: unknown, not resent
        if not args.no_start and result["status"] in ("pending", "waiting_quota"):
            kick(store)
        deadline = time.monotonic() + args.wait
        while result["status"] in ("pending", "running") and time.monotonic() < deadline and store.setting("operator"):
            time.sleep(1)
            result = store.get(result["id"])
            if result["status"] == "running" and recover(store, not args.no_start):
                result = store.get(result["id"])
        result["operator_configured"] = bool(store.setting("operator"))
    elif args.command == "status":
        from ob_operator import recover_orphaned
        recovered = recover_orphaned(store)
        result = store.get(args.id) if args.id else {"requests": store.list(args.project_id), "operator": store.setting("operator"),
                                                   "worker_running": recovered is None, "recovered": recovered or [],
                                                   "stop_requested": store.setting("stop_requested", False),
                                                   "retry_after": store.setting("retry_after")}
        # 現在在用哪一串、問過幾題、下一題會不會換串——不然「怎麼又是同一個對話」只能自己去翻 DB。
        if not args.id and args.project_id:
            live = store.current_conversation(args.project_id)
            due, why = store.rotate_due(args.project_id)
            result["conversation"] = live
            result["rotate_next"] = {"due": due, "why": why, "turns_limit": Store.ROTATE_TURNS, "days_limit": Store.ROTATE_DAYS}
            result["conversations"] = store.conversations(args.project_id)
    elif args.command == "rotate":
        # 不是現在就去開一串空的（那會留下一個沒人問過問題的對話）：標起來，下一題送出時才換。
        result = store.request_rotation(args.project_id)
    elif args.command == "recover":
        recovered = recover(store)
        result = {"worker_running": recovered is None, "recovered": recovered or []}
    elif args.command == "stop":
        store.set_setting("stop_requested", True)
        result = {"stop_requested": True, "note": "當前請求完成後停止；status 的 worker_running=false 才可更新／configure"}
    elif args.command == "work":
        from ob_operator import work
        try:
            work(store, args.once)
        except OBError as e:
            if str(e) != "busy":
                raise
            # 已經有一顆 worker 在跑：被 kick 出來的重複 worker 安靜退場，這是正常狀況（只有 `work` 這樣）。
            print(json.dumps({"busy": True, "note": "另一顆 worker 正在跑"}, ensure_ascii=False), file=sys.stderr)
        return 0
    elif args.command == "retry":
        result = store.retry(args.id)
        kick(store)
    elif args.command == "collect":
        from ob_operator import browser_consult
        if not store.setting("operator"):
            raise OBError("not_configured")
        recover(store)
        result = browser_consult(store, args.id, store.setting("operator"), collect=True)
    elif args.command == "resolve":
        result = resolve_unknown(store, args.id, args.confirmed_not_sent)
    elif args.command == "link":
        project = daemon_project(args.project_id)
        url = args.url
        if args.legacy_key:
            legacy = Path.home() / ".config/agents-manager/chatgpt-consult.json"
            url = json.loads(legacy.read_text()).get(args.legacy_key, {}).get("url")
        store.link(project["id"], project["label"], url)
        result = store.project(project["id"])
    if isinstance(result, dict):
        result.pop("claim_token", None)
    print(json.dumps(result, ensure_ascii=False))
    return 0


def cli(argv=None):
    """`main` 加上結束碼。`busy` 只有 `work` 算正常（那條自己吞掉並回 0）：其他子命令撞到忙碌時 stdout
    是空的，再回 0 的話，用退出碼判斷的呼叫端會以為對帳成功（review3 c4 L6）。"""
    try:
        return main(argv)
    except KeyboardInterrupt:
        return 130
    except Exception as e:
        print(json.dumps({"error": str(e)}, ensure_ascii=False), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(cli())
