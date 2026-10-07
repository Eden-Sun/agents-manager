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

/// 這台主機、這個實例的回收區（`<home>/<root>/bots-trash`），跟 bot 目錄使用同一個根。
async fn trash_dir(conn: &HostConn, remote_root: &str) -> Result<String> {
    Ok(trash_dir_for(&conn.home().await?, remote_root))
}

fn trash_dir_for(home: &str, remote_root: &str) -> String {
    format!("{home}/{remote_root}/bots-trash")
}

/// Check every component below the trusted home and pin later GC/restore paths to the opened directory.
fn trash_root_guard(home: &str, fallback: &str) -> String {
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
  case "$relative" in */*) relative=${{relative#*/}} ;; *) relative= ;; esac
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
    )
}

/// 把遠端的 `bots/<id>/` 搬進回收區。回搬到哪裡；目錄本來就不在回 `None`。
pub async fn move_in(conn: &HostConn, bot_id: &str, dir: &str, remote_root: &str) -> Result<Option<String>> {
    let dest = format!("{}/{bot_id}.{}", trash_dir(conn, remote_root).await?, now_ms());
    let script = format!(
        "set -e\nD={d}\nif [ -e \"$D\" ]; then\n  mkdir -p \"$(dirname {t})\"\n  mv \"$D\" {t}\n  printf 'AM_TRASHED\\n'\nelse\n  printf 'AM_NOTHING\\n'\nfi\n",
        d = sh_quote(&dir),
        t = sh_quote(&dest),
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
    let guard = trash_root_guard(&home, "printf 'AM_NONE\\n'");
    let script = format!(
        "set -e\nD={d}\nT={t}\ntrash=$T\n{guard}if [ -L \"$D\" ]; then rm \"$D\"; fi\nif [ -e \"$D\" ] || [ -L \"$D\" ]; then printf 'AM_KEPT\\n'; exit 0; fi\nbest=\nbm=0\nfor e in {id}.*; do\n  [ -d \"$e\" ] || continue\n  ms=${{e##*.}}\n  case \"$ms\" in ''|*[!0-9]*) continue;; esac\n  if [ \"$ms\" -gt \"$bm\" ]; then bm=$ms; best=$e; fi\ndone\nif [ -z \"$best\" ]; then printf 'AM_NONE\\n'; exit 0; fi\nmkdir -p \"$(dirname \"$D\")\"\nmv \"$best\" \"$D\"\nprintf 'AM_RESTORED %s/%s\\n' \"$trash\" \"$best\"\n",
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
pub async fn gc(conn: &HostConn, remote_root: &str, keep: Duration, max_bytes: u64) -> Result<(usize, usize)> {
    let cutoff = now_ms().saturating_sub(keep.as_millis());
    let max_kb = max_bytes / 1024;
    let home = conn.home().await?;
    let trash_dir = trash_dir_for(&home, remote_root);
    let guard = trash_root_guard(&home, "printf 'AM_TRASH_GC 0 0\\n'");
    // basename 是 `move_in` 造的 `<bot_id>.<毫秒>`（ULID，不含空白），所以第二道可以用行為單位排序；
    // 不是這個形狀的一律不動——那不是我們放的。`$T` 本身有空白也沒關係，只有 basename 進 sort。
    let script = format!(
        "T={t}\n{guard}C={cutoff}\nMAXK={max_kb}\nn=0\nev=0\n\
         # 列出「我們放的、可以按大小淘汰的」：<毫秒> <KB> <basename>。抽成函式是因為 `case` 的樣式\n\
         # 帶 `)`，寫在 $(...) 裡會被 shell 當成命令替換的結尾（macOS /bin/sh 實測語法錯誤）。\n\
         am_list() {{\n\
        \x20 for d in \"$T\"/*.*; do\n\
        \x20   [ -d \"$d\" ] || continue\n\
        \x20   b=${{d##*/}}; ms=${{b##*.}}\n\
        \x20   case \"$ms\" in ''|*[!0-9]*) continue;; esac\n\
        \x20   case \"$b\" in *[[:space:]]*) continue;; esac\n\
        \x20   printf '%s %s %s\\n' \"$ms\" \"$(du -sk \"$d\" 2>/dev/null | awk 'NR==1{{print $1+0}}')\" \"$b\"\n\
        \x20 done\n\
         }}\n\
         if [ -d \"$T\" ]; then\n\
        \x20 for d in \"$T\"/*.*; do\n\
        \x20   [ -d \"$d\" ] || continue\n\
        \x20   b=${{d##*/}}; ms=${{b##*.}}\n\
        \x20   case \"$ms\" in ''|*[!0-9]*) continue;; esac\n\
        \x20   if [ \"$ms\" -le \"$C\" ]; then rm -rf \"$d\" && n=$((n+1)); fi\n\
        \x20 done\n\
        \x20 list=$(am_list | sort -n)\n\
        \x20 total=$(printf '%s\\n' \"$list\" | awk '{{s+=$2}} END{{print s+0}}')\n\
        \x20 cnt=$(printf '%s\\n' \"$list\" | grep -c '[^[:space:]]')\n\
        \x20 i=0\n\
        \x20 for b in $(printf '%s\\n' \"$list\" | awk '{{print $3}}'); do\n\
        \x20   i=$((i+1))\n\
        \x20   [ \"$i\" -lt \"$cnt\" ] || break\n\
        \x20   [ \"$total\" -gt \"$MAXK\" ] || break\n\
        \x20   k=$(du -sk \"$T/$b\" 2>/dev/null | awk 'NR==1{{print $1+0}}')\n\
        \x20   if rm -rf \"$T/$b\"; then ev=$((ev+1)); total=$((total-k)); fi\n\
        \x20 done\n\
         fi\n\
         printf 'AM_TRASH_GC %s %s\\n' \"$n\" \"$ev\"\n",
        t = sh_quote(&trash_dir),
        guard = guard,
    );
    let out = conn.ssh_exec(&script).await?;
    out.lines()
        .find_map(|l| l.strip_prefix("AM_TRASH_GC "))
        .and_then(|rest| {
            let mut it = rest.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .ok_or_else(|| anyhow::anyhow!("remote bots-trash gc did not confirm: {}", out.trim()))
}
