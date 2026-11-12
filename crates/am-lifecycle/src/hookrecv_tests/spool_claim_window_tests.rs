use super::*;
use crate::testing as tt;

fn line(bot_id: &str, session: &str) -> String {
    let v = serde_json::json!({
        "bot_id": bot_id,
        "provider": "claude",
        "payload": {"hook_event_name": "Stop", "session_id": session},
    });
    format!("{v}\n")
}

async fn inbox_rows(env: &tt::Env) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM hook_events").fetch_one(&env.app.db).await.unwrap()
}

fn append(path: &std::path::Path, text: &str) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// 修正前：`read(spool)` → 整份寫回 `.replaying` → `remove_file(spool)`，中間那一段裡 hook 附加的行
/// 被最後那個 remove 連檔刪掉。注入點放在「摘下來之後、這一輪的副本還沒刪掉」那一瞬。
#[tokio::test]
async fn a_line_appended_between_claim_and_ack_survives() {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
    let dir = env.app.bot_dir(&bot.id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let spool = dir.join("hook-spool.jsonl");
    let staging = dir.join("hook-spool.jsonl.replaying");
    // 上一輪 ack 沒成功留下的 `.replaying`：修正前只有這條合併路徑會走到「讀完再刪」。
    std::fs::write(&staging, line(&bot.id, "s0")).unwrap();
    std::fs::write(&spool, line(&bot.id, "s1")).unwrap();

    let late_path = spool.clone();
    let late = line(&bot.id, "s2");
    crate::lifecycle::race_point::arm("spool_claimed", &bot.id, move || async move {
        append(&late_path, &late);
    });

    assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 2, "這一輪收的是 s0 與 s1");
    assert!(spool.exists(), "claim 之後附加的那一行必須還在 spool 上，不能被這一輪刪掉");
    assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 1, "下一輪把 s2 收進來");
    assert_eq!(inbox_rows(&env).await, 3, "三則都要進收件匣");
}

/// 摘下來（rename 成 `.claim`）之後、併進 `.replaying` 之前崩掉：`.claim` 是唯一的副本，
/// 下一輪要接著收，不能留在那裡沒人管。
#[tokio::test]
async fn a_leftover_claim_file_is_folded_in_on_the_next_round() {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
    let dir = env.app.bot_dir(&bot.id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let claim = dir.join("hook-spool.jsonl.claim");
    std::fs::write(&claim, line(&bot.id, "s0")).unwrap();
    assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 1, "只剩 .claim 也要收");
    assert!(!claim.exists(), "收完就不該留著");
    assert_eq!(inbox_rows(&env).await, 1);
}

fn fold_dir() -> std::path::PathBuf {
    let dir = crate::testing::track(std::env::temp_dir().join(format!("am-fold-{}", db::ulid())));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// #912：併進既有的 `.replaying` 是追加，不截斷；尾巴沒換行的半行補上換行再接；`src` 併完才刪。
#[test]
fn fold_spool_appends_without_truncating() {
    let dir = fold_dir();
    let (src, dst) = (dir.join("hook-spool.jsonl.claim"), dir.join("hook-spool.jsonl.replaying"));
    std::fs::write(&dst, "OLD1\nOLD2-half").unwrap();
    std::fs::write(&src, "NEW1\nNEW2\n").unwrap();
    super::fold_spool(&src, &dst).unwrap();
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), "OLD1\nOLD2-half\nNEW1\nNEW2\n");
    assert!(!src.exists(), "併完才刪 src");

    // dst 已經以換行結尾：不多補空行。
    std::fs::write(&src, "NEW3\n").unwrap();
    super::fold_spool(&src, &dst).unwrap();
    assert!(std::fs::read_to_string(&dst).unwrap().ends_with("NEW2\nNEW3\n"));

    // dst 不存在：直接搬過去。
    let fresh = dir.join("fresh.replaying");
    std::fs::write(&src, "ONLY\n").unwrap();
    super::fold_spool(&src, &fresh).unwrap();
    assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "ONLY\n");
    assert!(!src.exists());
}

/// #912：`src` 讀不到（這裡是目錄）就回錯，`dst` 一個位元組都不動。
#[test]
fn fold_spool_keeps_dst_when_src_is_unreadable() {
    let dir = fold_dir();
    let (src, dst) = (dir.join("hook-spool.jsonl.claim"), dir.join("hook-spool.jsonl.replaying"));
    std::fs::create_dir(&src).unwrap();
    std::fs::write(&dst, "KEEP-ME\n").unwrap();
    assert!(super::fold_spool(&src, &dst).is_err());
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), "KEEP-ME\n");
    assert!(src.exists());
}

/// #1068：超過上限的行整行丟掉、不留在記憶體；之後的行照常讀到；剛好到上限的行保留；最後一行沒換行也讀得到。
#[tokio::test]
async fn read_spool_line_drops_an_oversized_line_and_keeps_reading() {
    let mut data = vec![b'x'; super::MAX_SPOOL_LINE + 1];
    data.extend_from_slice(b"\n");
    data.extend(vec![b'y'; super::MAX_SPOOL_LINE]);
    data.extend_from_slice(b"\nok\ntail");
    let mut r = tokio::io::BufReader::with_capacity(4096, std::io::Cursor::new(data));
    let big = super::read_spool_line(&mut r).await.unwrap().unwrap();
    assert!(big.oversized);
    assert!(big.bytes.is_empty(), "超過上限的內容不保留");
    let edge = super::read_spool_line(&mut r).await.unwrap().unwrap();
    assert!(!edge.oversized, "剛好到上限的行不算超過");
    assert_eq!(edge.bytes.len(), super::MAX_SPOOL_LINE);
    let ok = super::read_spool_line(&mut r).await.unwrap().unwrap();
    assert_eq!(ok.bytes, b"ok");
    let tail = super::read_spool_line(&mut r).await.unwrap().unwrap();
    assert_eq!(tail.bytes, b"tail", "最後一行沒有換行也要讀到");
    assert!(super::read_spool_line(&mut r).await.unwrap().is_none());
}

// ---- 遠端那一半：真的把產出的腳本跑起來（不連任何主機，`$HOME` 指到暫存目錄）

struct Remote {
    home: std::path::PathBuf,
    bin: std::path::PathBuf,
    spool: std::path::PathBuf,
}

impl Remote {
    fn new() -> Self {
        let home = crate::testing::track(std::env::temp_dir().join(format!("am-claim-{}", db::ulid())));
        let d = home.join(crate::startup::REMOTE_ROOT).join("bots").join("botX");
        std::fs::create_dir_all(&d).unwrap();
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        Self { spool: d.join("hook-spool.jsonl"), home, bin }
    }

    /// `rm` 的替身：第一次被呼叫時先往 spool 附加一行，再真的刪。修正前的腳本第一個 `rm` 是
    /// `rm -f "$f"`（刪 live spool），那一行就此消失；修正後第一個 `rm` 是 `rm -f "$f.claim"`，
    /// 附加的行落在 `mv` 之後新建的 spool 上，刪不到。只處理 `$HOME` 底下的路徑。
    fn arm_append_inside_the_window(&self, text: &str) {
        let stub = format!(
            "#!/bin/sh\n\
             if [ ! -f \"$HOME/.armed\" ]; then : > \"$HOME/.armed\"; printf '%s' {line} >> {spool}; fi\n\
             for a in \"$@\"; do case \"$a\" in -*) ;; \"$HOME\"/*) ;; *) echo \"refusing $a\" >&2; exit 1;; esac; done\n\
             exec /bin/rm \"$@\"\n",
            line = crate::hosts::sh_quote(text),
            spool = crate::hosts::sh_quote(&self.spool.to_string_lossy()),
        );
        crate::testing::write_exec(self.bin.join("rm"), stub);
    }

    fn sh(&self, script: &str) -> String {
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap_or_default());
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("HOME", &self.home)
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(out.status.success(), "腳本失敗：{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// 遠端同款：claim 的 `cat` 與 `rm` 是兩支各自 fork／exec 的外部指令，中間附加的行不准被刪。
#[test]
fn the_remote_claim_script_keeps_a_line_appended_inside_the_window() {
    let r = Remote::new();
    let root = crate::startup::REMOTE_ROOT;
    // 上一輪 ack 沒成功留下的 `.replaying`：修正前只有這條路徑會 `cat` 完再刪 live spool。
    std::fs::write(r.spool.with_extension("jsonl.replaying"), "OLD\n").unwrap();
    std::fs::write(&r.spool, "A\n").unwrap();
    r.arm_append_inside_the_window("LATE\n");

    let out = r.sh(&claim_script("botX", root).unwrap());
    assert!(out.contains("OLD") && out.contains("A"), "兩則都要讀出來：{out}");
    r.sh(&ack_script("botX", root).unwrap());

    let left = std::fs::read_to_string(&r.spool).unwrap_or_default();
    assert!(left.contains("LATE"), "窗口裡附加的那一行要留在 spool 上：{left:?}");
    let again = r.sh(&claim_script("botX", root).unwrap());
    assert!(again.contains("LATE"), "下一輪要讀得到它：{again}");
    assert!(!again.contains("OLD"), "已經 ack 過的不能再出現：{again}");
}

/// #500：`am_fold` 沒收掉 `.claim`（`.replaying` 寫不進去）時，這一輪不准摘新的 spool——
/// 直接 `mv` 會把那份唯一的副本蓋掉，而且腳本 exit 0、stdout 有東西，daemon 會照常 ack。
/// 用「`.replaying` 是目錄」製造寫入失敗：跟權限無關，root 底下跑也一樣失敗。
#[test]
fn a_claim_that_could_not_be_folded_is_never_overwritten_by_the_next_spool() {
    let r = Remote::new();
    let root = crate::startup::REMOTE_ROOT;
    let claim = r.spool.with_extension("jsonl.claim");
    std::fs::write(&claim, "OLD-CLAIM\n").unwrap();
    std::fs::create_dir(r.spool.with_extension("jsonl.replaying")).unwrap();
    std::fs::write(&r.spool, "NEW\n").unwrap();

    r.sh(&claim_script("botX", root).unwrap());

    assert_eq!(std::fs::read_to_string(&claim).unwrap(), "OLD-CLAIM\n", "唯一的副本不准被蓋掉");
    assert_eq!(std::fs::read_to_string(&r.spool).unwrap(), "NEW\n", "新的 spool 留到下一輪");
}

/// #500 複看：`.claim` 併不進去時，卡住這件事要從遠端自己回報——腳本 exit 0、`n = 0`、
/// `drain_remote` 兩個 info 都被 `n > 0` 擋著，沒有這行標記就是一行 log 都沒有。
#[test]
fn a_claim_that_cannot_be_folded_reports_itself_as_stuck() {
    let r = Remote::new();
    let root = crate::startup::REMOTE_ROOT;
    std::fs::write(r.spool.with_extension("jsonl.claim"), "OLD-CLAIM\n").unwrap();
    std::fs::create_dir(r.spool.with_extension("jsonl.replaying")).unwrap();

    let out = r.sh(&claim_script("botX", root).unwrap());
    let d = parse_drain_output(&out);
    assert_eq!(d.stuck_bytes, Some(10), ".claim 的位元組數要帶回來：{out:?}");
    assert!(d.lines.is_empty(), "標記不可以被當成 spool 的一行：{:?}", d.lines);

    // 併得進去的那一輪不准報卡住（不然每次 drain 都在喊狼來了）。
    std::fs::remove_dir(r.spool.with_extension("jsonl.replaying")).unwrap();
    let out = r.sh(&claim_script("botX", root).unwrap());
    assert_eq!(parse_drain_output(&out).stuck_bytes, None, "{out:?}");
}

/// 連續看到才算數：併回去的那一輪要把計數清掉，不然一次卡住會永遠掛在那裡。
#[tokio::test]
async fn the_stuck_counter_only_counts_consecutive_rounds() {
    let env = crate::testing::env().await;
    let app = &env.app;
    let rounds = |app: &Arc<App>| {
        let app = app.clone();
        async move { app.spool_fold_stuck.lock().await.get("mac2/b1").map(|e| e.0) }
    };

    note_fold_stuck(app, "mac2", "b1", Some(8)).await;
    assert_eq!(rounds(app).await, Some(1));
    note_fold_stuck(app, "mac2", "b1", Some(16)).await;
    assert_eq!(rounds(app).await, Some(2));
    assert_eq!(app.spool_fold_stuck.lock().await.get("mac2/b1").map(|e| e.1), Some(16), "大小跟著最新一輪");

    note_fold_stuck(app, "mac2", "b1", None).await;
    assert_eq!(rounds(app).await, None, "收成了就重新算");
    note_fold_stuck(app, "mac2", "b1", Some(8)).await;
    assert_eq!(rounds(app).await, Some(1), "不是接著上次算");
}

/// #501：`.replaying` 是 claim 腳本自己建的，裡面裝完整 payload——不能交給那台機器的 umask。
/// 明確用寬鬆的 umask 跑，runner 剛好是 077 時才不會變成同義反覆。
#[test]
fn the_replaying_file_the_claim_script_creates_is_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let r = Remote::new();
    let root = crate::startup::REMOTE_ROOT;
    std::fs::write(&r.spool, "A\n").unwrap();
    r.sh(&format!("umask 022\n{}", claim_script("botX", root).unwrap()));
    let staging = r.spool.with_extension("jsonl.replaying");
    let mode = std::fs::metadata(&staging).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o600, ".replaying 只給自己讀");
}

/// An interrupted remote writer can leave a private `.tmp.*` file that replay never sees.
/// Drain should remove only old abandoned temp files and preserve a fresh writer's file.
#[test]
fn macos_local_remote_claim_cleans_stale_temp_spool_files() {
    let r = Remote::new();
    let root = crate::startup::REMOTE_ROOT;
    let sd = r.spool.parent().unwrap().join("hook-spool.d");
    std::fs::create_dir_all(&sd).unwrap();
    let stale = sd.join(".tmp.abandoned");
    let fresh = sd.join(".tmp.active");
    std::fs::write(&stale, "partial secret payload").unwrap();
    std::fs::write(&fresh, "active writer payload").unwrap();
    let status = std::process::Command::new("touch")
        .args(["-t", "200001010000"])
        .arg(&stale)
        .status()
        .unwrap();
    assert!(status.success());

    r.sh(&claim_script("botX", root).unwrap());

    assert!(!stale.exists(), "abandoned private temp files should be cleaned");
    assert!(fresh.exists(), "recent temp files may still belong to an active writer");
}

/// 摘下來還沒併進 `.replaying` 就斷線：`.claim` 下一輪要被收回來，不是留在遠端沒人管。
#[test]
fn the_remote_claim_script_folds_a_leftover_claim_file() {
    let r = Remote::new();
    let root = crate::startup::REMOTE_ROOT;
    std::fs::write(r.spool.with_extension("jsonl.claim"), "STRANDED\n").unwrap();
    let out = r.sh(&claim_script("botX", root).unwrap());
    assert!(out.contains("STRANDED"), "上一輪留下的 .claim 要收進來：{out}");
    assert!(!r.spool.with_extension("jsonl.claim").exists(), "併完就不留");
}

/// #652：一則一檔。claim 把已經寫好的 `*.json` 搬進 replaying 再讀；ack 才刪。
/// claim 展開 glob 之後才出現的檔留在 `hook-spool.d`，下一輪才收。
#[test]
fn macos_local_per_file_spool_is_claimed_then_acked() {
    let r = Remote::new();
    let root = crate::startup::REMOTE_ROOT;
    let dir = r.spool.parent().unwrap();
    let sd = dir.join("hook-spool.d");
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::write(sd.join("100-1.json"), "ONE\n").unwrap();
    std::fs::write(sd.join("200-2.json"), "TWO\n").unwrap();
    let out = r.sh(&claim_script("botX", root).unwrap());
    assert!(out.contains("ONE") && out.contains("TWO"), "{out}");
    assert!(!sd.join("100-1.json").exists(), "摘下來的不留在 live 目錄");
    let rd = dir.join("hook-spool.replaying");
    assert!(rd.join("100-1.json").exists() && rd.join("200-2.json").exists());
    std::fs::write(sd.join("300-3.json"), "LATE\n").unwrap();
    r.sh(&ack_script("botX", root).unwrap());
    assert!(!rd.join("100-1.json").exists(), "ack 才刪已收下的");
    assert_eq!(std::fs::read_to_string(sd.join("300-3.json")).unwrap(), "LATE\n");
    let again = r.sh(&claim_script("botX", root).unwrap());
    assert!(again.contains("LATE"), "{again}");
    assert!(!again.contains("ONE"), "ack 過的不能再出現：{again}");
}

/// 掃描要看得到只有一則一檔、還沒 claim 的 bot（舊 jsonl 不在時也要）。
#[test]
fn macos_local_scan_sees_a_per_file_spool() {
    let r = Remote::new();
    let root = crate::startup::REMOTE_ROOT;
    let dir = r.spool.parent().unwrap();
    let sd = dir.join("hook-spool.d");
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::write(sd.join("1-1.json"), "{}\n").unwrap();
    let out = r.sh(&scan_script(root));
    assert!(out.lines().any(|l| l.trim() == "botX"), "有檔就要被掃到：{out:?}");
    std::fs::remove_file(sd.join("1-1.json")).unwrap();
    let empty = r.sh(&scan_script(root));
    assert!(!empty.lines().any(|l| l.trim() == "botX"), "空目錄不是欠著：{empty:?}");
}

/// `.replaying` 尾巴沒有換行（崩在一行寫到一半）時，併進來的第一行不能跟它黏成一行（#302 的遠端版）。
#[test]
fn the_remote_fold_does_not_glue_lines_together() {
    let r = Remote::new();
    let root = crate::startup::REMOTE_ROOT;
    std::fs::write(r.spool.with_extension("jsonl.replaying"), "TORN").unwrap();
    std::fs::write(&r.spool, "NEXT\n").unwrap();
    let out = r.sh(&claim_script("botX", root).unwrap());
    assert!(out.lines().any(|l| l == "NEXT"), "新的一行要自己一行：{out:?}");
}
