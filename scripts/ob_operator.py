"""OB worker：不經任何 LLM，直接呼叫 `browser_consult()` 操作 ChatGPT 網頁（#1919）。

以前每筆請求先開一個 `claude --print --model sonnet`，只被要求呼叫一次 `mcp__ob__consult`，答案由
工具的持久收據給原任務 bot，模型輸出整個丟掉——那一層沒有判斷，只花訂閱額度。現在 worker 自己呼叫。
"""
import contextlib
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

from ob_store import OBError, Store, exclusive

# 等瀏覽器空出來最多這麼久：`collect` 最長佔住 720 秒，不等的話 worker 一撞上就把請求判成失敗
# （訊息還叫人去查登入），而那筆其實連送都沒送出去（review3 c4 L6）。
BROWSER_LOCK_WAIT = 720
# 等不到時的記號：`operate` 認得它就把請求放回佇列重排，不標 failed。
BUSY_MARK = "browser_busy"
# ego lite 根本沒在跑（GH #65）：跟 `BUSY_MARK` 同一招——`operate` 從 `browser_consult` 的錯誤訊息裡認這個字，
# 分出「瀏覽器沒開」跟其他失敗，訊息才講得出下一步。
BROWSER_NOT_RUNNING_MARK = "browser_not_running"
# 同一個行程對同一個 claim 只試一次：以前由 MCP 層擋第二次呼叫（`already_attempted`），現在換成這個集合。
ATTEMPT_MARK = "already_attempted"
_ATTEMPTED = set()

HERE = Path(__file__).resolve().parent


def clean_env():
    # 不繼承呼叫端的 bot hook／auth、模型、API key 或 Claude session：ego-browser 只需要基本 shell 環境。
    return {k: os.environ[k] for k in ("HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "LANG", "LC_ALL", "TZ", "SHELL") if k in os.environ}


def run_process(argv, *, cwd, env, timeout, stdin=None):
    proc = subprocess.Popen(argv, cwd=cwd, env=env, text=True, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        out, err = proc.communicate(stdin, timeout=timeout)
        return proc.returncode, out, err
    except BaseException:
        # Kill the whole process group, including MCP/browser children, on timeout or stop.
        try:
            os.killpg(proc.pid, signal.SIGTERM)
            proc.communicate(timeout=5)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.communicate()
        raise


def ego_lite_running():
    """ego lite 這個 app 有沒有在跑（GH #65）：查行程表，不去猜 `ego-browser` 連不上時印出的字——那是
    第三方 CLI 的訊息，會換版本、換語言。比對片段跟 daemon 的 `memstat` 分 app 用同一段
    （`ego lite.app/`），一台機器只認一種規則。查不出來（沒有 `ps`、逾時…）就當作在跑，照舊往下試，
    讓真正的錯誤浮出來，不要因為查詢本身失敗就誤報「沒開」。
    """
    try:
        out = subprocess.run(["ps", "-axo", "command"], capture_output=True, text=True, timeout=5).stdout
    except Exception:
        return True
    return "ego lite.app/" in out


def journal_for(store, ident):
    return store.root / (ident + ".browser.json")


# 真正送出去之後才算「可能已送出」。opened／preparing 是送出前的紀錄（#643）。
SENT_PHASES = ("dispatching", "sent")


def journal_may_have_sent(progress):
    return isinstance(progress, dict) and progress.get("phase") in SENT_PHASES


def journal_state(store, ident):
    path = journal_for(store, ident)
    try:
        progress = json.loads(path.read_text())
        if progress.get("url"):
            store.remember_url(ident, progress["url"])
        return progress
    except FileNotFoundError:
        return None


@contextlib.contextmanager
def browser_lock(store):
    """瀏覽器一次只給一筆請求用。等不到就回 `browser_busy`——這是「稍後再排」，不是「操作失敗」。"""
    holder = exclusive(store.root / "browser.lock", wait=BROWSER_LOCK_WAIT)
    try:
        fd = holder.__enter__()
    except OBError as e:
        if str(e) == "busy":
            raise OBError(f"{BUSY_MARK}：瀏覽器正被另一筆請求佔用（collect／resolve 或另一顆 worker），這次什麼都沒送出") from e
        raise
    try:
        yield fd
    except BaseException:
        holder.__exit__(*sys.exc_info())
        raise
    else:
        holder.__exit__(None, None, None)


def browser_consult(store, ident, config, collect=False, token=None):
    job = store.get(ident)
    if job["status"] == "done":
        return job
    if job["status"] != ("unknown" if collect else "running"):
        raise OBError("request_not_claimed")
    with browser_lock(store):
        # Re-check under browser.lock: resolve/re-claim may have run since, and an orphaned operator
        # of a dead worker must not send or finish for a later claim.
        job = store.get(ident)
        if job["status"] == "done":
            return job
        if collect and job["status"] != "unknown":
            raise OBError("request_not_unknown")
        if not collect and (job["status"] != "running" or not token or job["claim_token"] != token):
            raise OBError("request_not_claimed")
        token = None if collect else token
        progress = journal_state(store, ident)
        if progress and progress.get("phase") == "done":
            return store.finish(ident, progress["answer"], progress["url"], token)
        if progress and not collect and journal_may_have_sent(progress):
            raise OBError("delivery_unknown：不可再次送出，請 collect")
        project = store.project(job["project_id"])
        # 一個 project 一串，但那一串會輪替（SPEC：OB）。決定在派送前做完並記在列上：
        # `remember_url`／`finish` 要靠那個旗標分辨「這次是刻意換串」與「別的分頁冒充這個 project」。
        if not collect and not job["rotate"]:
            rotate, why = store.rotate_due(project["id"])
            if rotate:
                store.mark_rotate(ident)
                job = store.get(ident)
                print(f"ob: rotating conversation for {project['id']} ({why})", file=sys.stderr)
        rotating = bool(job["rotate"]) and not collect
        args = dict(project_id=project["id"], project_label=project["label"], question=job["question"], request_key=ident,
                    # 換串時不給 url：mjs 會在**同一個分頁**開一串新的（`previous_url` 認得那個分頁）。
                    url=None if rotating else project["url"],
                    previous_url=project["url"] if rotating else None,
                    journal=str(journal_for(store, ident)), collect=collect,
                    timeout_ms=600000)
        # 還沒送出任何東西：查得出瀏覽器根本沒開就直接講清楚，不要讓 ego-browser 自己去撞一個連不上的
        # 連線錯誤（那段訊息會被 stderr 丟掉，呼叫端只看到含糊的 browser_failed_or_interrupted）。
        if not ego_lite_running():
            raise OBError(f"{BROWSER_NOT_RUNNING_MARK}：ego lite 沒有在跑，請先開啟 ego lite 再重試")
        script = "globalThis.CONSULT_ARGS = " + json.dumps(args, ensure_ascii=False) + ";\n"
        script += (HERE / "chatgpt-consult.mjs").read_text()
        code, out, err = run_process([config["ego_binary"], "nodejs"], cwd=store.root,
                                     env=clean_env(), timeout=720, stdin=script)
        progress = journal_state(store, ident)
        if progress and progress.get("phase") == "done":
            return store.finish(ident, progress["answer"], progress["url"], token)
        # 只有瀏覽器 journal 的收據算答案（ego 的 stdout 不是證據）。非 0 才帶
        # stderr 最後一行：以前整段丟掉，呼叫端看到的永遠只有 code 而已（GH #65 初步線索）。
        if not code:
            raise OBError("browser_no_receipt")
        detail = next((line for line in reversed((err or "").strip().splitlines()) if line.strip()), "")
        raise OBError(f"browser_failed_or_interrupted：{detail[:200]}" if detail else "browser_failed_or_interrupted")


def attempt(store, ident, config, token):
    """每個 claim 只呼叫一次 `browser_consult`；同一行程再來一次就停（不再次操作瀏覽器）。"""
    key = (ident, token)
    if key in _ATTEMPTED and store.get(ident)["status"] != "done":
        raise OBError(f"{ATTEMPT_MARK}：停止，不再次操作")
    _ATTEMPTED.add(key)
    return browser_consult(store, ident, config, token=token)


def operate(store, job, config):
    # Every result is written for this claim only; a request re-claimed meanwhile is left to its new owner.
    ident, token = job["id"], job["claim_token"]
    try:
        error = ""
        try:
            attempt(store, ident, config, token)
        except OBError as e:
            error = str(e)
        if store.get(ident)["status"] == "done":
            return
        progress = journal_state(store, ident)
        if progress and progress.get("phase") == "done":
            store.finish(ident, progress["answer"], progress["url"], token)
        elif journal_may_have_sent(progress):
            store.fail(ident, "unknown", "可能已送出；用 collect 取回，不可重送", token)
        elif BUSY_MARK in error:
            # 瀏覽器被 collect／resolve 佔著，連送都沒送出：放回佇列重排，不要叫人去查登入（review3 c4 L6）。
            store.requeue(ident, "瀏覽器正被另一筆請求佔用，已放回佇列重排", token)
        elif BROWSER_NOT_RUNNING_MARK in error:
            # 沒有 journal：連送都沒送出，留在可重試的 failed，不能算 unknown（GH #65）。
            store.fail(ident, "failed", f"{BROWSER_NOT_RUNNING_MARK}：ego lite 沒有在跑，請先開啟 ego lite 再重試", token)
        elif progress:
            # opened／preparing：一個字都還沒送，可 retry，不能把同 project 佇列卡成 unknown。
            store.fail(ident, "failed", "還沒送出就失敗（journal 停在送出前）；檢查登入後 retry", token)
        else:
            store.fail(ident, "failed", (error or "browser_no_receipt")[:300] + "；檢查登入與 ego-browser，再 retry", token)
    except Exception as e:
        if store.get(ident)["status"] != "done":
            try:
                sent = journal_may_have_sent(journal_state(store, ident))
            except Exception:
                # 讀不到的 journal 不能當成「沒送出」再重送。
                sent = journal_for(store, ident).exists()
            store.fail(ident, "unknown" if sent else "failed", type(e).__name__, token)


def recover_under_lock(store):
    ids = store.recover()
    # 全部的 unknown，不能用 store.list()（只回最新 100 筆）：更舊的 unknown 也要保住它學到的 url。
    for (ident,) in store.db.execute("SELECT id FROM requests WHERE status='unknown'").fetchall():
        # Preserve a URL learned before the previous worker was interrupted.
        try:
            journal_state(store, ident)
        except Exception:
            pass  # unknown stays blocked; never retry on unreadable evidence
    return ids


def recover_orphaned(store):
    """Return None while a worker holds worker.lock, else the running ids turned unknown.

    Only a worker holding worker.lock claims, so a free lock proves every running row lost its worker.
    Such a request may already be in ChatGPT: it becomes unknown (collect/resolve), never pending.
    """
    try:
        with exclusive(store.root / "worker.lock"):
            return recover_under_lock(store)
    except OBError as e:
        if str(e) != "busy":
            raise
        return None


def work(store, once=False):
    config = store.setting("operator")
    if not config:
        raise OBError("not_configured：先 ob configure 指定 ego-browser")
    with exclusive(store.root / "worker.lock", wait=3):
        store.set_setting("stop_requested", False)
        recover_under_lock(store)
        def stop(_signal, _frame):
            raise SystemExit(0)
        signal.signal(signal.SIGTERM, stop)
        while True:
            if store.setting("stop_requested", False):
                return
            job = store.claim()
            if job:
                operate(store, job, config)
            if once:
                return
            if not job:
                time.sleep(5)
