//! 遠端 kill 守門熱更新（使用者 2026-11-28）：daemon 啟動／換版與主機重連時，不重啟 bot 就把守門推到遠端。
//!
//! 實測（claude 2.1.332、agy 1.5.18、grok 1.0.92）：三個 CLI 的 hook 設定檔（claude 的 `--settings`、agy 的
//! `~/.gemini/config/hooks.json`、grok 的 `<GROK_HOME>/hooks/*.json`）都只在啟動時讀一次，執行中改檔不會重載；
//! hook 指向的 `kill-guard.sh` 則是每次 PreToolUse 才執行，內容換掉立刻生效。所以：
//!
//! * `kill-guard.sh` 內容雜湊不同才寫（同目錄暫存檔＋`mv`、0700），寫了就對所有已指向它的 bot 即時生效。
//! * 執行中 bot 的設定檔內容不同才重寫，讓下次啟動拿到新版；但已經跑著的 CLI 看不到，所以設定裡的守門
//!   （PreToolUse）跟它啟動時載入的不同時，回報成「守門設定待重啟套用」，不自動重啟。
//! * 啟動時載入的守門版本記在 bot 目錄的 [`GUARD_SIG_FILE`]（啟動那一趟寫）。舊 daemon 啟動的 bot 沒有這個檔：
//!   claude 用它現在的設定檔推（那就是它啟動時讀的）；agy／grok 是共用的全域檔，這一趟真的改寫了才算待重啟。

use super::*;
use am_base::agy_support;
pub use am_base::remote_guard_pending::PendingRestart;

/// bot 目錄裡記「這顆 bot 啟動時載入的守門版本」的檔名。
pub const GUARD_SIG_FILE: &str = "guard.sig";

/// 舊 daemon 啟動、又推不出它載入了什麼版本時記的值；跟任何真的簽章都不相等，重啟後才會被蓋掉。
const UNKNOWN_SIG: &str = "unknown";

/// 這顆 binary 會給 `kind` 的遠端 bot 裝的 PreToolUse 守門的簽章（`short_hash` 是跨版本穩定的指紋）。
pub fn guard_sig_for(kind: &str, p: &RemoteHookPaths, bot: &db::Bot, time_zone: &str, instance: Option<&str>) -> Option<String> {
    let root = p.kill_guard.rsplit_once('/')?.0;
    let guard = remote_kill_guard_cmd(&p.kill_guard, kind);
    let pre = match kind {
        "claude" => remote_claude_settings(p, bot, time_zone)["hooks"]["PreToolUse"].clone(),
        "grok" => {
            let merged = super::grok_hook::hooks_json_merged_remote(None, &format!("{root}/{GROK_DISPATCH_SH}"), &guard);
            serde_json::from_str::<Value>(&merged).ok()?["hooks"]["PreToolUse"].clone()
        }
        "agy" => {
            let name = agy_support::hook_name(instance);
            let text = agy_support::hooks_merge_remote("", &name, &format!("{root}/{}", agy_support::DISPATCH_SH), &guard).ok()??;
            serde_json::from_str::<Value>(&text).ok()?[name.as_str()]["PreToolUse"].clone()
        }
        _ => return None,
    };
    Some(sig_of(&pre))
}

fn sig_of(pre_tool_use: &Value) -> String {
    am_base::supervisor_inbox::short_hash(pre_tool_use.to_string().as_bytes())
}

/// agy／grok 啟動時裝完全域 hook 之後記下載入的版本（claude 在 `install_remote_hook` 同一趟寫）。失敗只記 warn。
pub async fn note_started(conn: &HostConn, p: &RemoteHookPaths, bot: &db::Bot, time_zone: &str, instance: Option<&str>) {
    let Some(sig) = guard_sig_for(&bot.kind, p, bot, time_zone, instance) else { return };
    if let Err(e) = conn.ssh_exec(&write_script(&p.dir, None, Some(&sig))).await {
        tracing::warn!(host = %conn.name, bot = %bot.name, error = %format!("{e:#}"), "could not record the loaded kill-guard version");
    }
}

/// `kill-guard.sh` 雜湊相同就不寫（只補 0700）；遠端根目錄不存在（這台沒跑過 bot）就不建。
pub fn kill_guard_install_script(path: &str) -> String {
    use sha2::{Digest, Sha256};
    let guard = am_base::kill_guard::remote_kill_guard_sh();
    let hash = format!("{:x}", Sha256::digest(guard.as_bytes()));
    format!(
        "KG={kg}\nR=$(dirname \"$KG\")\n[ -d \"$R\" ] || {{ printf 'AM_KG_NOROOT\\n'; exit 0; }}\n\
         if [ -f \"$KG\" ] && [ ! -L \"$KG\" ]; then\n\
         if command -v sha256sum >/dev/null 2>&1; then NOW=$(sha256sum \"$KG\" | awk '{{print $1}}'); else NOW=$(shasum -a 256 \"$KG\" | awk '{{print $1}}'); fi\n\
         if [ \"$NOW\" = {hash} ]; then chmod 700 \"$KG\"; printf 'AM_KG_SAME\\n'; exit 0; fi\n\
         fi\n\
         set -e\numask 077\nKT=$(mktemp \"$KG.tmp.XXXXXX\")\ncat > \"$KT\" <<'AM_KILL_GUARD_EOF'\n{guard}AM_KILL_GUARD_EOF\nchmod 700 \"$KT\"\nmv -f \"$KT\" \"$KG\"\nprintf 'AM_KG_UPDATED\\n'\n",
        kg = sh_quote(path),
        hash = sh_quote(&hash),
    )
}

/// 讀 bot 目錄的啟動簽章與 claude 設定檔（base64）。bot 目錄不在就印 `AM_NO_DIR`。
fn read_script(dir: &str) -> String {
    format!(
        "D={d}\n[ -d \"$D\" ] || {{ printf 'AM_NO_DIR\\n'; exit 0; }}\n\
         printf 'AM_SIG:%s\\n' \"$(cat \"$D/{sig}\" 2>/dev/null)\"\n\
         if [ -f \"$D/claude-settings.json\" ] && [ ! -L \"$D/claude-settings.json\" ]; then printf 'AM_SETTINGS\\n'; base64 < \"$D/claude-settings.json\"; fi\n",
        d = sh_quote(dir),
        sig = GUARD_SIG_FILE,
    )
}

/// 同目錄暫存檔＋`mv`，0600。`settings`／`sig` 是 `None` 就不動那個檔。
fn write_script(dir: &str, settings: Option<&str>, sig: Option<&str>) -> String {
    let mut s = format!("set -e\numask 077\nD={}\n[ -d \"$D\" ]\n", sh_quote(dir));
    if let Some(text) = settings {
        s.push_str(&format!(
            "T=$(mktemp \"$D/.claude-settings.XXXXXX\")\ncat > \"$T\" <<'AM_SETTINGS_EOF'\n{text}AM_SETTINGS_EOF\nchmod 600 \"$T\"\nmv -f \"$T\" \"$D/claude-settings.json\"\n"
        ));
    }
    if let Some(sig) = sig {
        s.push_str(&format!(
            "T=$(mktemp \"$D/.guard-sig.XXXXXX\")\nprintf '%s' {} > \"$T\"\nchmod 600 \"$T\"\nmv -f \"$T\" \"$D/{GUARD_SIG_FILE}\"\n",
            sh_quote(sig)
        ));
    }
    s.push_str("printf 'AM_GUARD_WRITTEN\\n'\n");
    s
}

struct BotFiles {
    sig: Option<String>,
    settings: Option<String>,
}

fn parse_read(out: &str) -> anyhow::Result<Option<BotFiles>> {
    use base64::Engine as _;
    if out.lines().any(|l| l.trim() == "AM_NO_DIR") {
        return Ok(None);
    }
    let mut lines = out.lines();
    let sig = lines
        .find_map(|l| l.strip_prefix("AM_SIG:"))
        .ok_or_else(|| anyhow::anyhow!("remote guard read did not confirm:\n{}", out.trim()))?
        .trim()
        .to_string();
    let settings = match out.split_once("AM_SETTINGS\n") {
        Some((_, body)) => {
            let encoded: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
            Some(String::from_utf8(base64::engine::general_purpose::STANDARD.decode(encoded)?)?)
        }
        None => None,
    };
    Ok(Some(BotFiles { sig: (!sig.is_empty()).then_some(sig), settings }))
}

/// 一顆執行中的遠端 bot：`env` 是它的 pane env 裡跟 hook 位置有關的部分（`HOME`、`GROK_HOME`）。
pub struct GuardBot {
    pub bot: db::Bot,
    pub run_id: String,
    pub env: Value,
}

#[derive(Debug, Default)]
pub struct GuardRefresh {
    /// `kill-guard.sh` 這一趟有沒有換掉。
    pub guard_updated: bool,
    /// 這一趟改寫了設定檔的 bot 名稱。
    pub configs_updated: Vec<String>,
    pub pending: Vec<PendingRestart>,
    /// `(bot 名稱, 錯誤)`：這顆沒處理成，其他照做。
    pub failed: Vec<(String, String)>,
}

/// 一台已連線遠端主機的整趟：先換 `kill-guard.sh`，再逐顆處理執行中的 claude／agy／grok bot。
pub async fn refresh_host(conn: &HostConn, instance: Option<&str>, time_zone: &str, bots: &[GuardBot]) -> anyhow::Result<GuardRefresh> {
    let home = conn.home().await?;
    let kill_guard = format!("{home}/{}/{}", crate::hosts::remote_root_for(instance), am_base::kill_guard::REMOTE_KILL_GUARD_SH);
    let out = conn.ssh_exec(&kill_guard_install_script(&kill_guard)).await?;
    let mut r = GuardRefresh::default();
    if out.contains("AM_KG_UPDATED") {
        r.guard_updated = true;
    } else if !out.contains("AM_KG_SAME") && !out.contains("AM_KG_NOROOT") {
        anyhow::bail!("remote kill-guard install did not confirm:\n{}", out.trim());
    }
    for b in bots {
        match refresh_bot(conn, instance, time_zone, b).await {
            Ok((changed, pending)) => {
                if changed {
                    r.configs_updated.push(b.bot.name.clone());
                }
                if pending {
                    r.pending.push(PendingRestart { bot_id: b.bot.id.clone(), bot_name: b.bot.name.clone(), kind: b.bot.kind.clone(), run_id: b.run_id.clone() });
                }
            }
            Err(e) => r.failed.push((b.bot.name.clone(), format!("{e:#}"))),
        }
    }
    Ok(r)
}

/// 回 `(設定檔有沒有改寫, 是否待重啟)`。
async fn refresh_bot(conn: &HostConn, instance: Option<&str>, time_zone: &str, b: &GuardBot) -> anyhow::Result<(bool, bool)> {
    let bot = &b.bot;
    let p = remote_bot_dir_for(conn, &bot.id, instance).await?;
    let Some(want) = guard_sig_for(&bot.kind, &p, bot, time_zone, instance) else { return Ok((false, false)) };
    let Some(files) = parse_read(&conn.ssh_exec(&read_script(&p.dir)).await?)? else { return Ok((false, false)) };
    let (changed, baseline) = match bot.kind.as_str() {
        "claude" => {
            let text = format!("{}\n", serde_json::to_string_pretty(&remote_claude_settings(&p, bot, time_zone))?);
            let changed = files.settings.as_deref() != Some(text.as_str());
            if changed {
                let out = conn.ssh_exec(&write_script(&p.dir, Some(&text), None)).await?;
                if !out.contains("AM_GUARD_WRITTEN") {
                    anyhow::bail!("remote claude settings write did not confirm:\n{}", out.trim());
                }
            }
            let loaded = files.settings.as_deref().and_then(|t| serde_json::from_str::<Value>(t).ok()).map(|v| sig_of(&v["hooks"]["PreToolUse"]));
            (changed, loaded.unwrap_or_else(|| UNKNOWN_SIG.into()))
        }
        "grok" => {
            let changed = install_remote_grok_hook(conn, &b.env, instance).await?;
            (changed, if changed { UNKNOWN_SIG.into() } else { want.clone() })
        }
        "agy" => {
            let changed = crate::agy_remote::install_remote(conn, instance, b.env.get("HOME").and_then(Value::as_str)).await?;
            (changed, if changed { UNKNOWN_SIG.into() } else { want.clone() })
        }
        _ => return Ok((false, false)),
    };
    let started = match files.sig {
        Some(s) => s,
        None => {
            let out = conn.ssh_exec(&write_script(&p.dir, None, Some(&baseline))).await?;
            if !out.contains("AM_GUARD_WRITTEN") {
                anyhow::bail!("remote guard signature write did not confirm:\n{}", out.trim());
            }
            baseline
        }
    };
    Ok((changed, started != want))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(script: &str, home: &std::path::Path) -> String {
        let out = Command::new("/bin/sh").arg("-c").arg(script).env("HOME", home).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn mode(p: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn kill_guard_is_written_only_when_its_hash_differs() {
        let home = crate::test_dirs::track(std::env::temp_dir().join(format!("am-kg-{}", crate::db::ulid())));
        let root = home.join(crate::hosts::REMOTE_ROOT);
        let kg = root.join(am_base::kill_guard::REMOTE_KILL_GUARD_SH);
        let script = kill_guard_install_script(&kg.to_string_lossy());

        assert_eq!(sh(&script, &home).trim(), "AM_KG_NOROOT", "沒跑過 bot 的主機不建目錄");
        assert!(!root.exists());

        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&kg, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(sh(&script, &home).trim(), "AM_KG_UPDATED");
        assert_eq!(std::fs::read_to_string(&kg).unwrap(), am_base::kill_guard::remote_kill_guard_sh());
        assert_eq!(mode(&kg), 0o700);

        let before = std::fs::metadata(&kg).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(sh(&script, &home).trim(), "AM_KG_SAME", "雜湊相同不重寫");
        assert_eq!(std::fs::metadata(&kg).unwrap().modified().unwrap(), before);
        assert!(std::fs::read_dir(&root).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".tmp.")), "不留暫存檔");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_read_and_write_scripts_round_trip_settings_and_signature() {
        let home = crate::test_dirs::track(std::env::temp_dir().join(format!("am-kg-{}", crate::db::ulid())));
        let dir = home.join("bots/b1");
        let d = dir.to_string_lossy().into_owned();
        assert!(parse_read(&sh(&read_script(&d), &home)).unwrap().is_none(), "沒有 bot 目錄就跳過");

        std::fs::create_dir_all(&dir).unwrap();
        let empty = parse_read(&sh(&read_script(&d), &home)).unwrap().unwrap();
        assert_eq!((empty.sig, empty.settings), (None, None));

        let text = "{\n  \"hooks\": {}\n}\n";
        assert!(sh(&write_script(&d, Some(text), Some("abc123")), &home).contains("AM_GUARD_WRITTEN"));
        let back = parse_read(&sh(&read_script(&d), &home)).unwrap().unwrap();
        assert_eq!(back.sig.as_deref(), Some("abc123"));
        assert_eq!(back.settings.as_deref(), Some(text));
        assert_eq!(mode(&dir.join("claude-settings.json")), 0o600);
        assert_eq!(mode(&dir.join(GUARD_SIG_FILE)), 0o600);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn each_kind_has_a_stable_guard_signature_that_tracks_the_guard_command() {
        let bot = super::super::model_args_tests::bot("claude", None, None, false);
        let p = |kg: &str| RemoteHookPaths { dir: "/h/r/bots/b".into(), hook_sh: "/h/r/bots/b/hook.sh".into(), settings: "/h/r/bots/b/claude-settings.json".into(), kill_guard: kg.into() };
        for kind in ["claude", "agy", "grok"] {
            let a = guard_sig_for(kind, &p("/h/r/kill-guard.sh"), &bot, "Asia/Taipei", None).unwrap();
            assert_eq!(Some(a.clone()), guard_sig_for(kind, &p("/h/r/kill-guard.sh"), &bot, "Europe/Paris", None), "{kind}: 只看守門，時區不影響");
            assert_ne!(Some(a), guard_sig_for(kind, &p("/h/other/kill-guard.sh"), &bot, "Asia/Taipei", None), "{kind}: 守門指令變了簽章要變");
        }
        assert_eq!(guard_sig_for("codex", &p("/h/r/kill-guard.sh"), &bot, "Asia/Taipei", None), None);
    }
}
