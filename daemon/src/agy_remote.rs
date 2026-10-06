//! 遠端主機上的 agy bot 的 hook 安裝（SPEC §12a.12）。本機版在 `lifecycle::agy_hook`；這裡是同一件事經 ssh 做在那台的 `$HOME`：
//!
//! * `~/<遠端根>/agy-hook.sh`：dispatcher。agy 在 `~/.gemini/config/hooks.json` 的具名 hook 與 `settings.json` 的 `statusLine` 都指向它。
//!   pane env 沒有 `AM_BOT_ID`／`AM_HOOK_TOKEN`（或 `AM_INSTANCE` 不符）就什麼都不做——使用者自己開的 agy 不受影響；hook 事件永遠印 `{}`、
//!   exit 0（hook 是同步的、stdout 會被當成決策，statusLine 什麼都不印）。
//! * dispatcher 把 payload 轉給那顆 bot 的 `hook.sh agy <bot> -`（同 claude／codex／grok 的遠端 spool，daemon 掃回來）。agy 的 payload 沒有事件名、
//!   `Stop` 也沒有回覆文字，本機由 `hook` 子行程補（`hook_cmd::enrich_agy_payload`，讀 transcript 尾端）；遠端沒有那個子行程，所以 dispatcher
//!   用 python3 做**同一件事**（事件名＋最近一回合的問答＋`lastInputTokens`），沒有 python3 就只補事件名（回合照收，沒有回覆文字）。
//! * `hooks.json` 的具名鍵與 `settings.json` 的 `statusLine` 用 [`crate::trust::update_remote_file`] merge：只動我們那一項、其他原樣、
//!   讀不懂就不覆寫、讀到寫之間被改過就重讀（agy 自己與使用者也在寫這兩個檔）。
//!
//! 全部走遠端的絕對家目錄（`conn.home()`），不假設 ssh 那條 shell 的 `$HOME`。

use crate::agy_support as cfg;
use crate::hosts::{sh_quote, HostConn};
use anyhow::{bail, Result};

/// dispatcher 裡的 python：補事件名；`Stop` 另外讀 transcript 尾端（2 MiB）把最近一回合的問答放進去。
/// 必須跟 `agy_support::{last_exchange,last_input_tokens}` 同一份語意（`tests` 的對照測試把兩邊餵同一份 fixture）。
/// 不能含單引號：它被包在 sh 的單引號裡。
const ENRICH_PY: &str = r#"import json,sys
ev=sys.argv[1] if len(sys.argv)>1 else ""
try:
    p=json.loads(sys.stdin.buffer.read().decode("utf-8","replace"))
except Exception:
    raise SystemExit
if not isinstance(p,dict):
    raise SystemExit
if ev:
    p["hookEventName"]=ev
def user_text(v):
    if v.get("type")!="USER_INPUT" or not isinstance(v.get("content"),str):
        return None
    c=v["content"]
    a=c.find("<USER_REQUEST>")
    if a<0:
        return None
    a+=len("<USER_REQUEST>")
    b=c.find("</USER_REQUEST>",a)
    return c[a:b].strip() if b>=0 else None
def assistant_text(v):
    t=v.get("type")
    if not isinstance(t,str):
        return None
    t=t.upper()
    if "PLANNER_RESPONSE" not in t and "NOTIFY_USER" not in t:
        return None
    c=v.get("content")
    if isinstance(c,dict):
        c=next((c[k] for k in ("response","text","message","content","notification") if isinstance(c.get(k),str)),None)
    if not isinstance(c,str):
        return None
    c=c.strip()
    return c or None
if ev=="Stop":
    path=p.get("transcriptPath")
    if isinstance(path,str) and path.endswith(".jsonl"):
        try:
            with open(path,"rb") as f:
                f.seek(0,2)
                n=f.tell()
                f.seek(max(0,n-2097152))
                data=f.read()
            user=asst=tokens=None
            for line in data.decode("utf-8","replace").splitlines():
                try:
                    v=json.loads(line)
                except Exception:
                    continue
                if not isinstance(v,dict):
                    continue
                u=user_text(v)
                if u is not None:
                    user=u
                    asst=None
                    continue
                a=assistant_text(v)
                if a is not None:
                    asst=a
                    t=v.get("input_tokens")
                    tokens=t if isinstance(t,int) and not isinstance(t,bool) and t>0 else tokens
            if tokens is not None:
                p["lastInputTokens"]=tokens
            if asst is not None:
                p["lastAssistantMessage"]=asst
            if user is not None:
                p["lastUserMessage"]=user
        except Exception:
            pass
sys.stdout.write(json.dumps(p,separators=(",",":")))
"#;

/// 遠端 dispatcher 的內容。`root`／`instance` 跟遠端 `hook.sh` 同一組（隔離實例有自己的根與閘門）。
pub fn dispatch_sh(root: &str, instance: Option<&str>) -> String {
    debug_assert!(!ENRICH_PY.contains('\''), "the python is wrapped in sh single quotes");
    let gate = match instance {
        Some(slug) => format!("[ \"${{AM_INSTANCE:-}}\" = {} ] || {{ cat >/dev/null 2>&1; am_reply; exit 0; }}\n", sh_quote(slug)),
        None => "[ -z \"${AM_INSTANCE:-}\" ] || { cat >/dev/null 2>&1; am_reply; exit 0; }\n".to_string(),
    };
    format!(
        "#!/bin/sh\n\
         # agents-manager agy dispatcher (SPEC §12a.12, remote host). Installed by the daemon; no-op outside daemon panes.\n\
         # agy runs this synchronously and reads stdout as a decision: hook events always answer `{{}}`, the status line prints nothing.\n\
         AM_EVENT=$(printf '%s' \"${{1:-}}\" | tr -cd 'A-Za-z')\n\
         am_reply() {{ [ \"$AM_EVENT\" = state ] || printf '{{}}\\n'; }}\n\
         [ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || {{ cat >/dev/null 2>&1; am_reply; exit 0; }}\n\
         {gate}\
         H=\"$HOME/{root}/bots/$AM_BOT_ID/hook.sh\"\n\
         [ -x \"$H\" ] || {{ cat >/dev/null 2>&1; am_reply; exit 0; }}\n\
         PAYLOAD=$(head -c 1048576)\n\
         [ -n \"$PAYLOAD\" ] || PAYLOAD='{{}}'\n\
         if command -v python3 >/dev/null 2>&1; then\n\
         \x20 OUT=$(printf '%s' \"$PAYLOAD\" | python3 -c '{py}' \"$AM_EVENT\" 2>/dev/null)\n\
         \x20 case \"$OUT\" in '{{'*) PAYLOAD=\"$OUT\" ;; esac\n\
         else\n\
         \x20 case \"$PAYLOAD\" in\n\
         \x20   '{{}}') PAYLOAD=\"{{\\\"hookEventName\\\":\\\"$AM_EVENT\\\"}}\" ;;\n\
         \x20   '{{'*) PAYLOAD=\"{{\\\"hookEventName\\\":\\\"$AM_EVENT\\\",${{PAYLOAD#\\{{}}\" ;;\n\
         \x20 esac\n\
         fi\n\
         printf '%s' \"$PAYLOAD\" | \"$H\" agy \"$AM_BOT_ID\" - >/dev/null 2>&1 || true\n\
         am_reply\n\
         exit 0\n",
        gate = gate,
        root = root,
        py = ENRICH_PY,
    )
}

/// 寫 dispatcher：同目錄暫存檔 → `chmod 700` → `mv`（原子；agy 正在跑 hook 也不會讀到半個檔）。
fn write_dispatcher_script(path: &str, text: &str) -> String {
    let mut delim = String::from("AM_AGY_DISPATCH_EOF");
    while text.contains(&delim) {
        delim.push('_');
    }
    format!(
        "set -e\numask 077\nW={w}\nmkdir -p \"$(dirname \"$W\")\"\nWT=$(mktemp \"$W.tmp.XXXXXX\")\ntrap 'rm -f \"$WT\"' EXIT\ncat > \"$WT\" <<'{delim}'\n{text}{delim}\nchmod 700 \"$WT\"\nmv -f \"$WT\" \"$W\"\nprintf 'AM_AGY_DISPATCH_OK\\n'\n",
        w = sh_quote(path),
    )
}

/// bot 啟動（遠端）：裝／修 dispatcher 與兩個設定檔，回「有沒有改到東西」。失敗由呼叫端記 warn、bot 照常啟動
/// （少的只是 hook 回報，畫面判讀還在——跟本機一樣）。
pub async fn install_remote(conn: &HostConn, instance: Option<&str>) -> Result<bool> {
    let home = conn.home().await?;
    let root = crate::startup::remote_root_for(instance);
    let dispatcher = format!("{home}/{root}/{}", cfg::DISPATCH_SH);
    let text = dispatch_sh(&root, instance);
    let out = conn.ssh_exec(&write_dispatcher_script(&dispatcher, &text)).await?;
    if !out.contains("AM_AGY_DISPATCH_OK") {
        bail!("remote agy dispatcher install did not confirm:\n{}", out.trim());
    }
    let name = cfg::hook_name(instance);
    let mut changed = crate::trust::update_remote_file(conn, &format!("{home}/.gemini/config/hooks.json"), |existing| {
        cfg::hooks_merge(existing, &name, &dispatcher)
    })
    .await?;
    let statusline = format!("{} state", sh_quote(&dispatcher));
    changed |= crate::trust::update_remote_file(conn, &format!("{home}/.gemini/antigravity-cli/settings.json"), |existing| {
        cfg::statusline_merge(existing, &statusline)
    })
    .await?;
    tracing::info!(host = %conn.name, dispatcher, changed, "remote agy hooks installed");
    Ok(changed)
}

#[cfg(test)]
mod tests;
