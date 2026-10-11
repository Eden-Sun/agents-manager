//! 遠端 bot 目錄的回收區（issue #411）：刪 bot 時遠端的 `bots/<id>/` 不 `rm -rf`，而是 ssh 搬到同一個根底下的
//! `bots-trash/<id>.<毫秒>/`；`POST /api/bots/{id}/restore` 時 ssh 搬回來；主機連上（開機、重連）時清掉放超過
//! [`crate::bot_trash::KEEP_DAYS`] 天的。
//!
//! #406 只替本機做了回收區（`bot_trash`）。遠端誤刪後還原得回 bot 列與 config，遠端目錄裡的東西（hook spool 裡還沒重放的
//! 事件、shim、手動放的檔）卻拿不回來。規則跟本機一樣：名字裡的時間戳決定過期（`mv` 不更新目錄 mtime）；還原時
//! `bots/<id>/` 已經在（重新啟動過、重建了）就不動，免得蓋掉新的。根目錄照 SPEC §3.1 遠端分實例（`remote_root_for`）。
//! ssh 失敗照 `remote_purge` 既有的補帳：`purge_bot_dir` 記失敗，下一輪掃描再搬一次（冪等：已經不在就什麼都不做）。

use crate::hosts::{sh_quote, HostConn};
use anyhow::{bail, Result};
use std::time::Duration;

/// 還原時等 ssh 多久：在 bot 鎖裡、擋著 HTTP 回應，不用 `ssh_exec` 預設的 30 秒。
const RESTORE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()
}

fn trash_dir_for(home: &str, remote_root: &str) -> String {
    format!("{home}/{remote_root}/bots-trash")
}

/// Check every component below the trusted home and pin later GC/restore paths to the opened directory.
fn trash_root_guard(home: &str, fallback: &str, create: bool) -> String {
    // `create`：寫入前（move_in）回收區的各段還不存在就建出來；每一段都先確認不是 symlink，才建或才進（#1299）。
    let mk = if create { "  [ -e \"$current\" ] || mkdir \"$current\" || trash_untrusted\n" } else { "" };
    format!(
        r#"H={home}
trash_untrusted() {{ {fallback}; exit 0; }}
case "$H" in /*) ;; *) trash_untrusted ;; esac
case "$T" in "$H"/*) ;; *) trash_untrusted ;; esac
relative=${{T#"$H"/}}
current="$H"
while [ -n "$relative" ]; do
  component=${{relative%%/*}}
  case "$component" in ''|.|..) trash_untrusted ;; esac
  current="$current/$component"
  [ ! -L "$current" ] || trash_untrusted
{mk}  case "$relative" in */*) relative=${{relative#*/}} ;; *) relative= ;; esac
done
if [ ! -d "$T" ]; then trash_untrusted; fi
home_real=$(CDPATH= cd "$H" 2>/dev/null && pwd -P) || trash_untrusted
trash_real=$(CDPATH= cd "$T" 2>/dev/null && pwd -P) || trash_untrusted
expected="$home_real/${{T#"$H"/}}"
[ "$trash_real" = "$expected" ] || trash_untrusted
CDPATH= cd "$T" 2>/dev/null || trash_untrusted
T=.
"#,
        home = sh_quote(home),
        fallback = fallback,
        mk = mk,
    )
}

/// 把遠端的 `bots/<id>/` 搬進回收區。回搬到哪裡；目錄本來就不在回 `None`。
pub async fn move_in(conn: &HostConn, bot_id: &str, dir: &str, remote_root: &str) -> Result<Option<String>> {
    let home = conn.home().await?;
    let trash = trash_dir_for(&home, remote_root);
    let name = format!("{bot_id}.{}", now_ms());
    let dest = format!("{trash}/{name}");
    // 搬進去之前先驗回收區的每一段：不是 symlink、實際落在家目錄底下（#1299）。驗不過就不動 `$D`，回報失敗。
    // 目錄還沒有就在驗過的路徑上建出來（`create`）。`D` 不存在就不驗、不建，免得為了「沒東西要搬」建出空的回收區。
    let guard = trash_root_guard(&home, "printf 'AM_UNTRUSTED\\n'", true);
    let script = format!(
        "set -e\nD={d}\nif [ ! -e \"$D\" ]; then\n  printf 'AM_NOTHING\\n'\n  exit 0\nfi\nT={t}\n{guard}NAME={name}\nif [ -e \"$NAME\" ] || [ -L \"$NAME\" ]; then printf 'AM_UNTRUSTED\\n'; exit 0; fi\nmv \"$D\" \"$NAME\"\nprintf 'AM_TRASHED\\n'\n",
        d = sh_quote(&dir),
        t = sh_quote(&trash),
        name = sh_quote(&name),
        guard = guard,
    );
    let out = conn.ssh_exec(&script).await?;
    if out.contains("AM_TRASHED") {
        Ok(Some(dest))
    } else if out.contains("AM_NOTHING") {
        Ok(None)
    } else {
        bail!("remote move to bots-trash did not confirm: {}", out.trim())
    }
}

/// 還原：遠端 `bots/<id>/` 還不在時，把回收區裡這顆最新的那份搬回去。symlink 只拆連結再搬回；真目錄或檔案已在回 `None`。
pub async fn restore(conn: &HostConn, bot_id: &str, dir: &str, remote_root: &str) -> Result<Option<String>> {
    let home = conn.home().await?;
    let trash_dir = trash_dir_for(&home, remote_root);
    let guard = trash_root_guard(&home, "printf 'AM_NONE\\n'", false);
    let script = format!(
        "set -e\nD={d}\nT={t}\ntrash=$T\n{guard}B={id}\nbest=\nbm=0\n# 只認 `<id>.<毫秒>` 整段：`<id>.attachments.<毫秒>` 之類是別的回收項目，不能當成 bot 目錄搬回去（#1312）。\nfor e in \"$B\".*; do\n  [ -d \"$e\" ] || continue\n  ms=${{e#\"$B.\"}}\n  case \"$ms\" in ''|*[!0-9]*) continue;; esac\n  if [ \"$ms\" -gt \"$bm\" ]; then bm=$ms; best=$e; fi\ndone\n# 先確定有備份才動 `$D`（#1311）：沒有就什麼都不碰，連結原樣留著。\nif [ -z \"$best\" ]; then printf 'AM_NONE\\n'; exit 0; fi\nlink_aside=\nif [ -L \"$D\" ]; then\n  link_aside=\"$D.am-link-$$\"\n  mv \"$D\" \"$link_aside\"\nfi\nif [ -e \"$D\" ] || [ -L \"$D\" ]; then\n  [ -z \"$link_aside\" ] || mv \"$link_aside\" \"$D\"\n  printf 'AM_KEPT\\n'; exit 0\nfi\nmkdir -p \"$(dirname \"$D\")\"\nif mv \"$best\" \"$D\"; then\n  [ -z \"$link_aside\" ] || rm \"$link_aside\"\n  printf 'AM_RESTORED %s/%s\\n' \"$trash\" \"$best\"\nelse\n  [ -z \"$link_aside\" ] || mv \"$link_aside\" \"$D\"\n  exit 1\nfi\n",
        d = sh_quote(&dir),
        t = sh_quote(&trash_dir),
        guard = guard,
        id = sh_quote(bot_id),
    );
    let out = conn.ssh_exec_timeout(&script, RESTORE_TIMEOUT).await?;
    if let Some(from) = out.lines().find_map(|l| l.strip_prefix("AM_RESTORED ")) {
        Ok(Some(from.trim().to_string()))
    } else if out.contains("AM_KEPT") || out.contains("AM_NONE") {
        Ok(None)
    } else {
        bail!("remote restore from bots-trash did not confirm: {}", out.trim())
    }
}

/// 兩道一起跑，跟本機 [`crate::bot_trash::gc_with_cap`] 同一套政策（#441）：先清放超過 `keep` 的
/// （看名字裡的時間，不看 mtime——`mv` 不更新目錄 mtime），再看總量，還超過 `max_bytes` 就從**最舊的**
/// 開始清到降下來；最新那一份永遠留著（剛刪掉的那顆才是最可能要還原的）。回 `(過期清掉幾份, 因為超量再清掉幾份)`。
///
/// 只有時間規則不夠：#141／#196 那兩次遠端磁碟被塞爆都是「清得不夠快」，而七天之內連刪十幾顆 bot 時，
/// 時間規則一份都不會清。
///
/// **跟 [`restore`] 不互斥**：`restore` 挑中一份剛好跨過期限的、而這裡在它 `mv` 之前就 `rm -rf` 掉，
/// `restore` 的 `set -e` 會讓腳本失敗、[`restore_for`] 記一行 warn 不擋還原。窗口極窄（那一份本來下一輪
/// 也要過期），代價是那個目錄拿不回來，不值得為它加鎖。
/// `known`＝DB 認得的 bot id（#1313）：回收區裡只有 `<known id>.<毫秒>` 這類才是我們放的，別人的目錄不動、不算進總量。
pub async fn gc(conn: &HostConn, remote_root: &str, keep: Duration, max_bytes: u64, known: &[String]) -> Result<(usize, usize)> {
    let cutoff = now_ms().saturating_sub(keep.as_millis());
    let max_kb = max_bytes / 1024;
    let home = conn.home().await?;
    let trash_dir = trash_dir_for(&home, remote_root);
    let guard = trash_root_guard(&home, "printf 'AM_TRASH_GC 0 0\\n'", false);
    // 「我們放的」才看：`<bot_id>.<毫秒>`（或 `<bot_id>.attachments.<毫秒>`、`<bot_id>.share_workspace.<毫秒>`），
    // `<bot_id>` 必須在 `KNOWN` 裡（DB 認得的 bot，#1313）。`backups.1`、`X.1` 這種別人放的，不管尾碼多小、多舊都不動、也不算進總量。
    // `am_owned` 印出毫秒、不是我們的就回非 0；抽成函式是因為 `case` 的樣式帶 `)`，寫在 $(...) 裡會被 shell 當成命令替換的結尾（macOS /bin/sh 實測語法錯誤）。
    let known_sh = sh_quote(&known.join(" "));
    let script = format!(
        "T={t}\n{guard}C={cutoff}\nMAXK={max_kb}\nKNOWN={known_sh}\nn=0\nev=0\nunk=0\n\
         am_owned() {{\n\
         b=$1; id=${{b%%.*}}; rest=${{b#*.}}\n\
         [ \"$id\" != \"$b\" ] || return 1\n\
         case \" $KNOWN \" in *\" $id \"*) ;; *) return 1;; esac\n\
         case \"$rest\" in\n\
         *.*) kind=${{rest%.*}}; ms=${{rest##*.}}\n\
         case \"$kind\" in attachments|share_workspace) ;; *) return 1;; esac;;\n\
         *) ms=$rest;;\n\
         esac\n\
         case \"$ms\" in ''|*[!0-9]*) return 1;; esac\n\
         printf '%s\\n' \"$ms\"\n\
         }}\n\
         am_size() {{\n\
         s=$(du -sk \"$1\" 2>/dev/null) || return 1\n\
         set -- $s\n\
         case \"$1\" in ''|*[!0-9]*) return 1;; esac\n\
         printf '%s\\n' \"$1\"\n\
         }}\n\
         am_list() {{\n\
         for d in \"$T\"/*; do\n\
         [ -d \"$d\" ] || continue\n\
         b=${{d##*/}}\n\
         ms=$(am_owned \"$b\") || continue\n\
         case \"$b\" in *[[:space:]]*) continue;; esac\n\
         k=$(am_size \"$d\") || k=-\n\
         printf '%s %s %s\\n' \"$ms\" \"$k\" \"$b\"\n\
         done\n\
         }}\n\
         if [ -d \"$T\" ]; then\n\
         for d in \"$T\"/*; do\n\
         [ -d \"$d\" ] || continue\n\
         b=${{d##*/}}\n\
         ms=$(am_owned \"$b\") || continue\n\
         if [ \"$ms\" -le \"$C\" ]; then rm -rf \"$d\" && n=$((n+1)); fi\n\
         done\n\
         list=$(am_list | sort -n)\n\
         unk=$(printf '%s\\n' \"$list\" | awk '$2==\"-\"{{n++}} END{{print n+0}}')\n\
         total=$(printf '%s\\n' \"$list\" | awk '{{s+=$2}} END{{print s+0}}')\n\
         cnt=$(printf '%s\\n' \"$list\" | grep -c '[^[:space:]]')\n\
         i=0\n\
         for b in $(printf '%s\\n' \"$list\" | awk '{{print $3}}'); do\n\
         i=$((i+1))\n\
         [ \"$i\" -lt \"$cnt\" ] || break\n\
         [ \"$unk\" -eq 0 ] || break\n\
         [ \"$total\" -gt \"$MAXK\" ] || break\n\
         k=$(am_size \"$T/$b\") || break\n\
         if rm -rf \"$T/$b\"; then ev=$((ev+1)); total=$((total-k)); fi\n\
         done\n\
         fi\n\
         printf 'AM_TRASH_GC %s %s %s\\n' \"$n\" \"$ev\" \"$unk\"\n",
        t = sh_quote(&trash_dir),
        guard = guard,
    );
    let out = conn.ssh_exec(&script).await?;
    let (expired, evicted, unknown) = out
        .lines()
        .find_map(|l| l.strip_prefix("AM_TRASH_GC "))
        .and_then(|rest| {
            let mut it = rest.split_whitespace();
            Some((it.next()?.parse::<usize>().ok()?, it.next()?.parse::<usize>().ok()?, it.next().and_then(|u| u.parse::<usize>().ok()).unwrap_or(0)))
        })
        .ok_or_else(|| anyhow::anyhow!("remote bots-trash gc did not confirm: {}", out.trim()))?;
    // `du` 讀不到的那一份大小不知道：這一輪不用總量淘汰，等能量到再說（#1300）。
    if unknown > 0 {
        tracing::warn!(unknown, "remote bots-trash entries could not be measured; no size-based eviction this round");
    }
    Ok((expired, evicted))
}
