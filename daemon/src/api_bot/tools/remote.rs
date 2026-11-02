//! 遠端主機專案的 claude-api 工具（使用者 2026-11-02 裁示：遠端不做沙箱，直接用該主機 ssh 帳號的權限執行，
//! 跟遠端的 claude／codex bot 一樣；本機（Linux）仍走 bwrap，見 `bash.rs`）。
//!
//! 每次 bash 呼叫都是獨立的一次 ssh：cd、export、背景行程不會延續。檔案編輯也是 ssh 讀寫，
//! 路徑限制在專案目錄底下（純字串檢查，`..` 一律拒絕；`/workspace/…` 是專案根目錄的別名，讓 prompt 兩邊寫法一致）。
//! 這道檢查只擋「編輯器」誤用，不是安全邊界：bash 本來就有該帳號的全部權限。
use super::bash::Limits;
use crate::api_bot::protocol::ToolCall;
use am_base::hosts::{sh_quote, HostConn};
use futures::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_FILE: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 16 * 1024;
const MAX_COMMAND: usize = 64 * 1024;
const WORKSPACE_ALIAS: &str = "workspace";
/// ssh 本身（連線、啟動 shell）比指令多留的秒數；指令的逾時由外層 `tokio::time::timeout` 負責。
const SSH_SLACK: Duration = Duration::from_secs(5);

pub struct RemoteTools {
    conn: Arc<HostConn>,
    root: String,
    limits: Limits,
}

impl RemoteTools {
    pub fn new(conn: Arc<HostConn>, root: &str, limits: Limits) -> Result<Self, String> {
        limits.validate()?;
        let root = normalize_root(root)?;
        Ok(Self { conn, root, limits })
    }

    fn ensure_connected(&self) -> Result<(), String> {
        if self.conn.is_connected() {
            Ok(())
        } else {
            Err(format!("host `{}` is not connected", self.conn.name))
        }
    }

    async fn bash(&self, command: &str, cancel: &CancellationToken) -> Result<String, String> {
        if let am_base::kill_guard::Verdict::Deny(reason) = am_base::kill_guard::check_command(command) {
            return Err(reason);
        }
        if command.len() > MAX_COMMAND {
            return Err("command is too large".into());
        }
        self.ensure_connected()?;
        let marker = format!("__AM_API_EXIT_{}__", crate::db::ulid());
        let script = bash_script(&self.root, command, &marker, self.limits.output_bytes);
        let run = self.conn.ssh_exec_path_timeout(&script, self.limits.timeout + SSH_SLACK);
        let out = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("cancelled".into()),
            r = tokio::time::timeout(self.limits.timeout, run) => match r {
                Err(_) => return Err("command timed out".into()),
                Ok(r) => r.map_err(|e| format!("remote command could not run: {e}"))?,
            },
        };
        let (body, code) = parse_bash_reply(&out, &marker, self.limits.output_bytes)?;
        if code != 0 {
            return Err(format!("command failed (exit {code}): {body}"));
        }
        Ok(body)
    }

    async fn read(&self, rel: &str) -> Result<ReadReply, String> {
        self.ensure_connected()?;
        let out = self
            .conn
            .ssh_exec_timeout(&read_script(&self.root, rel), self.limits.timeout + SSH_SLACK)
            .await
            .map_err(|e| format!("remote read failed: {e}"))?;
        parse_read_reply(&out)
    }

    async fn write(&self, script: String, data: &str) -> Result<(), String> {
        self.ensure_connected()?;
        let out = self
            .conn
            .ssh_exec_stdin(&script, data.as_bytes(), self.limits.timeout + SSH_SLACK)
            .await
            .map_err(|e| format!("remote write failed: {e}"))?;
        parse_write_reply(&out)
    }

    async fn editor(&self, input: &Value) -> Result<String, String> {
        let command = input.get("command").and_then(Value::as_str).ok_or("missing editor command")?;
        let path = input.get("path").and_then(Value::as_str).ok_or("missing path")?;
        let field = |key: &str| input.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing {key}"));
        if !matches!(command, "view" | "create" | "str_replace" | "insert") {
            return Err("unsupported editor command".into());
        }
        let rel = resolve_path(&self.root, path)?;
        if command == "create" {
            let content = field("file_text")?;
            if content.len() > MAX_FILE {
                return Err("file size limit exceeded".into());
            }
            if rel.is_empty() {
                return Err("file path required".into());
            }
            self.write(create_script(&self.root, &rel), content).await?;
            return Ok("created".into());
        }
        let text = match self.read(&rel).await? {
            ReadReply::Missing => return Err("path cannot be resolved".into()),
            ReadReply::Dir(names) => {
                if command != "view" {
                    return Err("only single-link regular files are editable".into());
                }
                if input.get("view_range").is_some() {
                    return Err("view_range requires a file".into());
                }
                return render_dir(names);
            }
            ReadReply::File(text) => text,
        };
        if command == "view" {
            return render_view(&text, input.get("view_range"));
        }
        let updated = if command == "str_replace" {
            apply_str_replace(&text, field("old_str")?, field("new_str")?)?
        } else {
            let line = input.get("insert_line").and_then(Value::as_u64).ok_or("invalid insert_line")? as usize;
            apply_insert(&text, line, field("new_str")?)?
        };
        if updated.len() > MAX_FILE {
            return Err("file size limit exceeded".into());
        }
        self.write(replace_script(&self.root, &rel), &updated).await?;
        Ok("updated".into())
    }
}

impl crate::api_bot::runner::Tools for RemoteTools {
    fn execute<'a>(&'a self, call: &'a ToolCall, cancel: &'a CancellationToken) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            match call.name.as_str() {
                "bash" => {
                    if call.input.get("restart").and_then(Value::as_bool) == Some(true) {
                        return Err("persistent shell restart is unsupported".into());
                    }
                    let command = call.input.get("command").and_then(Value::as_str).ok_or("missing command")?;
                    self.bash(command, cancel).await
                }
                "str_replace_based_edit_tool" => {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => Err("cancelled".into()),
                        r = self.editor(&call.input) => r,
                    }
                }
                _ => Err("unsupported tool".into()),
            }
        })
    }
}

// ---------------------------------------------------------------------------------------------
// 以下是純函式（不碰 ssh），單元測試直接測。

/// 專案目錄：去掉結尾 `/`；必須是絕對路徑或 `~`／`~/…`（遠端 home 由 shell 展開）。
fn normalize_root(root: &str) -> Result<String, String> {
    let r = root.trim();
    if r.contains('\0') || r.is_empty() {
        return Err("invalid project directory".into());
    }
    if !(r.starts_with('/') || r == "~" || r.starts_with("~/")) {
        return Err("project directory must be absolute".into());
    }
    let trimmed = r.trim_end_matches('/');
    Ok(if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() })
}

/// 路徑字串轉成 shell 單字：開頭的 `~` 換成 `"$HOME"`，其餘一律單引號（空白、`;`、`$(…)` 都是資料）。
fn shell_path(path: &str) -> String {
    if path == "~" {
        return "\"$HOME\"".into();
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"{}", sh_quote(&format!("/{rest}"))),
        None => sh_quote(path),
    }
}

fn join_root(root: &str, rel: &str) -> String {
    match (rel.is_empty(), root) {
        (true, r) => r.to_string(),
        (false, "/") => format!("/{rel}"),
        (false, r) => format!("{r}/{rel}"),
    }
}

fn components(path: &str) -> Vec<&str> {
    path.split('/').filter(|c| !c.is_empty() && *c != ".").collect()
}

/// 編輯器路徑 → 專案目錄底下的相對路徑（空字串＝專案根目錄本身）。
/// 相對路徑視為專案內；絕對路徑只收「專案目錄本身開頭」與 `/workspace` 別名；任何 `..` 一律拒絕。
pub(super) fn resolve_path(root: &str, given: &str) -> Result<String, String> {
    if given.is_empty() || given.contains('\0') {
        return Err("invalid path".into());
    }
    let parts = components(given);
    if parts.contains(&"..") {
        return Err("invalid path traversal".into());
    }
    let absolute = given.starts_with('/') || given == "~" || given.starts_with("~/");
    let rest: &[&str] = if absolute {
        let root_parts = components(root);
        if parts.starts_with(&root_parts) {
            &parts[root_parts.len()..]
        } else if given.starts_with('/') && parts.first() == Some(&WORKSPACE_ALIAS) {
            &parts[1..]
        } else {
            return Err("path outside project".into());
        }
    } else {
        &parts
    };
    Ok(rest.join("/"))
}

/// bash 的整段腳本。`ssh_exec` 把腳本餵進遠端 `sh -s` 的 stdin，所以指令要 `</dev/null`（否則讀 stdin 的指令會吃掉腳本）；
/// 指令的 stdout／stderr 合併後最多留 `limit+1` 位元組（多一個用來判斷有沒有被截），exit code 另外寫進暫存檔，
/// 最後印 `\n<marker><code>\n`——ssh 對非 0 的結束碼會丟掉 stdout，所以腳本自己永遠以 0 結束。
fn bash_script(root: &str, command: &str, marker: &str, limit: usize) -> String {
    let inner = format!("cd {} || exit 97\n{command}", shell_path(root));
    format!(
        "rcf=\"${{TMPDIR:-/tmp}}/.am-api-rc.$$\"\n\
         {{ /bin/bash --noprofile --norc -c {} </dev/null 2>&1; echo $? > \"$rcf\"; }} | head -c {}\n\
         rc=$(cat \"$rcf\" 2>/dev/null); rm -f \"$rcf\"\n\
         printf '\\n%s%s\\n' {} \"${{rc:-255}}\"\n",
        sh_quote(&inner),
        limit + 1,
        sh_quote(marker),
    )
}

/// 解析 [`bash_script`] 的輸出 → (輸出本文, exit code)。超過 `limit` 的本文截斷並註明；找不到 marker＝腳本沒跑完。
fn parse_bash_reply(out: &str, marker: &str, limit: usize) -> Result<(String, i32), String> {
    let at = out.rfind(marker).ok_or("remote shell did not report an exit status")?;
    let code: i32 = out[at + marker.len()..].trim().parse().map_err(|_| "remote shell reported a bad exit status")?;
    let body = &out[..at];
    let body = body.strip_suffix('\n').unwrap_or(body);
    if body.len() <= limit {
        return Ok((body.to_string(), code));
    }
    let mut cut = limit;
    while !body.is_char_boundary(cut) {
        cut -= 1;
    }
    Ok((format!("{}\n[output truncated at {limit} bytes]", &body[..cut]), code))
}

#[derive(Debug, PartialEq, Eq)]
enum ReadReply {
    Missing,
    Dir(Vec<String>),
    File(String),
}

/// 讀檔／目錄：第一行回報種類（`AM_DIR`／`AM_FILE <bytes>`／`AM_MISSING`），腳本永遠以 0 結束。
/// 檔案過大不 cat；含 NUL 的當成二進位拒絕（寫回去會壞掉）。
fn read_script(root: &str, rel: &str) -> String {
    let p = shell_path(&join_root(root, rel));
    format!(
        "p={p}\n\
         if [ -d \"$p\" ]; then echo AM_DIR; ls -A -- \"$p\"\n\
         elif [ -f \"$p\" ]; then s=$(wc -c < \"$p\" | tr -d ' ')\n\
           if [ \"$s\" -gt {MAX_FILE} ]; then echo \"AM_BIG $s\"\n\
           elif [ \"$(tr -d '\\000' < \"$p\" | wc -c | tr -d ' ')\" != \"$s\" ]; then echo AM_BINARY\n\
           else echo \"AM_FILE $s\"; cat -- \"$p\"; fi\n\
         else echo AM_MISSING; fi\n\
         exit 0\n"
    )
}

fn parse_read_reply(out: &str) -> Result<ReadReply, String> {
    let (head, rest) = out.split_once('\n').unwrap_or((out, ""));
    let head = head.trim_end();
    if head == "AM_MISSING" {
        return Ok(ReadReply::Missing);
    }
    if head == "AM_DIR" {
        return Ok(ReadReply::Dir(rest.lines().map(str::to_string).collect()));
    }
    if head == "AM_BINARY" {
        return Err("file read failed or not UTF-8".into());
    }
    if head.starts_with("AM_BIG ") {
        return Err("file size limit exceeded".into());
    }
    if let Some(size) = head.strip_prefix("AM_FILE ") {
        let size: usize = size.trim().parse().map_err(|_| "remote read returned a bad size")?;
        if rest.len() != size || rest.contains('\u{fffd}') {
            // 長度對不上＝讀到一半被改或有非 UTF-8 位元組（lossy 解碼後會變長）。
            return Err("file read failed or not UTF-8".into());
        }
        return Ok(ReadReply::File(rest.to_string()));
    }
    Err("remote read returned an unexpected reply".into())
}

/// create：`set -C`（noclobber）讓 `>` 變成 O_EXCL——檔案已存在就不寫（跟本機版一樣不覆蓋）；父目錄要存在。
fn create_script(root: &str, rel: &str) -> String {
    let p = shell_path(&join_root(root, rel));
    format!(
        "p={p}\n\
         [ -d \"$(dirname -- \"$p\")\" ] || {{ echo AM_NOPARENT; exit 0; }}\n\
         [ ! -e \"$p\" ] && [ ! -L \"$p\" ] || {{ echo AM_EXISTS; exit 0; }}\n\
         set -C\n\
         cat > \"$p\" && echo AM_OK || echo AM_FAIL\n\
         exit 0\n"
    )
}

/// str_replace／insert 寫回：原地覆寫（保留權限與 inode），只收既有一般檔。
fn replace_script(root: &str, rel: &str) -> String {
    let p = shell_path(&join_root(root, rel));
    format!(
        "p={p}\n\
         [ -f \"$p\" ] || {{ echo AM_MISSING; exit 0; }}\n\
         cat > \"$p\" && echo AM_OK || echo AM_FAIL\n\
         exit 0\n"
    )
}

fn parse_write_reply(out: &str) -> Result<(), String> {
    match out.trim() {
        "AM_OK" => Ok(()),
        "AM_NOPARENT" => Err("path cannot be resolved".into()),
        "AM_EXISTS" => Err("path open refused".into()),
        "AM_MISSING" => Err("path cannot be resolved".into()),
        _ => Err("file write failed".into()),
    }
}

fn render_dir(mut names: Vec<String>) -> Result<String, String> {
    names.retain(|n| !n.is_empty() && n != "." && n != "..");
    names.sort();
    let joined = names.join("\n");
    if joined.len() > MAX_OUTPUT {
        return Err("directory output limit exceeded".into());
    }
    Ok(joined)
}

/// 跟本機 editor 的 view 同格式：`<行號>\t<內容>`，可帶 `[起, 迄]`（迄 -1＝到尾）。
fn render_view(text: &str, range: Option<&Value>) -> Result<String, String> {
    use std::fmt::Write;
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = match range {
        None => (0, lines.len()),
        Some(range) => {
            let range = range.as_array().filter(|a| a.len() == 2).ok_or("invalid view_range")?;
            let start = range[0].as_i64().filter(|n| *n >= 1).ok_or("invalid view_range")?;
            let end = range[1].as_i64().ok_or("invalid view_range")?;
            let end = if end == -1 { lines.len() as i64 } else { end };
            if end < start || start > lines.len() as i64 || end > lines.len() as i64 {
                return Err("invalid view_range".into());
            }
            (start as usize - 1, end as usize)
        }
    };
    let mut result = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let _ = writeln!(result, "{}\t{}", start + i + 1, line);
        if result.len() > MAX_OUTPUT {
            return Err("view output limit exceeded; use view_range".into());
        }
    }
    Ok(result)
}

fn apply_str_replace(text: &str, old: &str, new: &str) -> Result<String, String> {
    if old.is_empty() || text.matches(old).count() != 1 {
        return Err("old_str must match exactly once".into());
    }
    Ok(text.replacen(old, new, 1))
}

fn apply_insert(text: &str, line: usize, new: &str) -> Result<String, String> {
    let offset = if line == 0 {
        0
    } else {
        let mut lines = text.split_inclusive('\n');
        let mut offset = 0;
        for _ in 0..line {
            offset += lines.next().ok_or("invalid insert_line")?.len();
        }
        offset
    };
    let prefix = if offset > 0 && !text[..offset].ends_with('\n') { "\n" } else { "" };
    let mut result = text.to_string();
    result.insert_str(offset, &format!("{prefix}{new}\n"));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const ROOT: &str = "/Users/u/proj";

    #[test]
    fn resolve_path_keeps_edits_inside_the_project() {
        assert_eq!(resolve_path(ROOT, "src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(resolve_path(ROOT, "./src//a.rs").unwrap(), "src/a.rs");
        assert_eq!(resolve_path(ROOT, "/Users/u/proj/src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(resolve_path(ROOT, "/Users/u/proj").unwrap(), "");
        assert_eq!(resolve_path(ROOT, "/workspace/src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(resolve_path(ROOT, "/workspace").unwrap(), "");
        for bad in ["../x", "src/../../x", "/workspace/../etc/passwd", "/Users/u/proj/../other/x", "/etc/passwd", "/Users/u/proj2/x", "/Users/u", "~/other", "", "a\0b"] {
            assert!(resolve_path(ROOT, bad).is_err(), "{bad:?} must be rejected");
        }
        // 專案目錄自己在 home 底下的 `~` 寫法
        assert_eq!(resolve_path("~/proj", "~/proj/a").unwrap(), "a");
        assert!(resolve_path("~/proj", "~/proj2/a").is_err());
    }

    #[test]
    fn root_must_be_absolute_and_trailing_slash_is_dropped() {
        assert_eq!(normalize_root("/Users/u/proj/").unwrap(), "/Users/u/proj");
        assert_eq!(normalize_root("~/proj").unwrap(), "~/proj");
        assert_eq!(normalize_root("/").unwrap(), "/");
        assert!(normalize_root("proj").is_err());
        assert!(normalize_root("").is_err());
    }

    #[test]
    fn shell_path_quotes_data_and_expands_only_a_leading_tilde() {
        assert_eq!(shell_path("/a b/$(x)"), "'/a b/$(x)'");
        assert_eq!(shell_path("~"), "\"$HOME\"");
        assert_eq!(shell_path("~/my proj"), "\"$HOME\"'/my proj'");
        assert_eq!(shell_path("it's"), "'it'\\''s'");
        assert_eq!(join_root("/", "a"), "/a");
        assert_eq!(join_root(ROOT, ""), ROOT);
    }

    #[test]
    fn bash_reply_parses_exit_code_and_ignores_spoofed_markers() {
        let m = "__AM_API_EXIT_X__";
        assert_eq!(parse_bash_reply(&format!("hello\n\n{m}0\n"), m, 100).unwrap(), ("hello\n".into(), 0));
        assert_eq!(parse_bash_reply(&format!("\n{m}7\n"), m, 100).unwrap(), ("".into(), 7));
        // 腳本補的那個換行要被吃掉，輸出本身的換行保留
        assert_eq!(parse_bash_reply(&format!("a\nb\n\n{m}1\n"), m, 100).unwrap(), ("a\nb\n".into(), 1));
        assert_eq!(parse_bash_reply(&format!("a\n{m}1\n"), m, 100).unwrap(), ("a".into(), 1));
        // 使用者輸出裡出現 marker 字樣：只認最後一個
        assert_eq!(parse_bash_reply(&format!("{m}9\nx\n{m}0\n"), m, 100).unwrap().1, 0);
        assert!(parse_bash_reply("no marker", m, 100).is_err());
        assert!(parse_bash_reply(&format!("x\n{m}abc\n"), m, 100).is_err());
    }

    #[test]
    fn bash_reply_truncates_on_a_char_boundary() {
        let m = "__M__";
        let (body, code) = parse_bash_reply(&format!("{}\n{m}0\n", "字".repeat(10)), m, 7).unwrap();
        assert_eq!(code, 0);
        assert!(body.starts_with("字字"), "{body}");
        assert!(body.contains("[output truncated at 7 bytes]"));
        assert!(!body.contains("字字字"));
    }

    /// 用本機 `/bin/sh` 真的跑一次 bash 腳本（跟遠端 `sh -s` 一樣從 stdin 餵）。
    fn run_script(script: &str) -> String {
        let mut child = Command::new("/bin/sh").arg("-s").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "script must always exit 0: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn bash_script_runs_in_the_project_dir_and_reports_exit_code() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-remote-tools-{}", crate::db::ulid())));
        let proj = dir.join("my proj");
        std::fs::create_dir_all(&proj).unwrap();
        let root = proj.to_str().unwrap();
        let m = "__AM_T__";
        let go = |cmd: &str, limit: usize| parse_bash_reply(&run_script(&bash_script(root, cmd, m, limit)), m, limit).unwrap();
        let (out, code) = go("pwd; echo err >&2; touch made", 4096);
        assert_eq!(code, 0);
        assert!(out.contains("my proj") && out.contains("err"), "{out}");
        assert!(proj.join("made").exists());
        assert_eq!(go("echo before; exit 3", 4096), ("before\n".into(), 3));
        // 讀 stdin 的指令不能吃掉後面的腳本
        assert_eq!(go("cat; echo after", 4096), ("after\n".into(), 0));
        // 輸出太大：被截斷，但 exit code 還在
        let (out, code) = go("yes x | head -c 100000; exit 5", 50);
        assert_eq!(code, 5);
        assert!(out.contains("[output truncated at 50 bytes]"), "{out}");
        // 專案目錄不存在：97
        let missing = parse_bash_reply(&run_script(&bash_script("/no/such/dir-am", "echo hi", m, 100)), m, 100).unwrap();
        assert_eq!(missing.1, 97);
        assert!(!missing.0.contains("hi"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_and_write_scripts_round_trip_through_sh() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-remote-edit-{}", crate::db::ulid())));
        let proj = dir.join("p q");
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        let root = proj.to_str().unwrap();
        std::fs::write(proj.join("f.txt"), "one\ntwo\n").unwrap();
        std::fs::write(proj.join("bin"), b"a\0b").unwrap();
        std::fs::write(proj.join("sub/x"), "").unwrap();
        let read = |rel: &str| parse_read_reply(&run_script(&read_script(root, rel)));
        assert_eq!(read("f.txt").unwrap(), ReadReply::File("one\ntwo\n".into()));
        assert_eq!(read("nope").unwrap(), ReadReply::Missing);
        assert!(read("bin").is_err());
        let ReadReply::Dir(names) = read("").unwrap() else { panic!() };
        assert_eq!(render_dir(names).unwrap(), "bin\nf.txt\nsub");
        // create：不覆蓋既有檔、父目錄要存在
        let write = |script: String, data: &str| {
            let mut child = Command::new("/bin/sh").arg("-c").arg(&script).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
            use std::io::Write;
            // 拒絕的腳本（檔已存在、父目錄不在）不讀 stdin 就先退出，寫入可能撞 EPIPE：那是預期的，結果看 stdout。
            let _ = child.stdin.take().unwrap().write_all(data.as_bytes());
            parse_write_reply(&String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout))
        };
        assert!(write(create_script(root, "sub/new.txt"), "hi\n").is_ok());
        assert_eq!(std::fs::read_to_string(proj.join("sub/new.txt")).unwrap(), "hi\n");
        assert!(write(create_script(root, "f.txt"), "clobber").is_err());
        assert_eq!(std::fs::read_to_string(proj.join("f.txt")).unwrap(), "one\ntwo\n");
        assert!(write(create_script(root, "no/dir/x"), "x").is_err());
        // 寫回：只收既有檔
        assert!(write(replace_script(root, "f.txt"), "three\n").is_ok());
        assert_eq!(std::fs::read_to_string(proj.join("f.txt")).unwrap(), "three\n");
        assert!(write(replace_script(root, "ghost"), "x").is_err());
        assert!(!proj.join("ghost").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_reply_rejects_unexpected_shapes() {
        assert_eq!(parse_read_reply("AM_MISSING\n").unwrap(), ReadReply::Missing);
        assert!(parse_read_reply("AM_BIG 99999999\n").is_err());
        assert!(parse_read_reply("AM_FILE 3\nab").is_err(), "長度對不上");
        assert!(parse_read_reply("AM_FILE 4\na\u{fffd}").is_err(), "非 UTF-8");
        assert!(parse_read_reply("garbage").is_err());
        assert!(parse_read_reply("").is_err());
    }

    #[test]
    fn view_matches_the_local_editor_format() {
        assert_eq!(render_view("a\nb\nc\n", None).unwrap(), "1\ta\n2\tb\n3\tc\n");
        let r = |v: Value| render_view("a\nb\nc\n", Some(&v));
        assert_eq!(r(serde_json::json!([2, 3])).unwrap(), "2\tb\n3\tc\n");
        assert_eq!(r(serde_json::json!([2, -1])).unwrap(), "2\tb\n3\tc\n");
        assert!(r(serde_json::json!([0, 1])).is_err());
        assert!(r(serde_json::json!([3, 2])).is_err());
        assert!(r(serde_json::json!([1, 9])).is_err());
        assert!(render_view(&"x\n".repeat(20_000), None).is_err());
    }

    #[test]
    fn str_replace_and_insert_follow_the_local_rules() {
        assert_eq!(apply_str_replace("a b c", "b", "X").unwrap(), "a X c");
        assert!(apply_str_replace("a b b", "b", "X").is_err());
        assert!(apply_str_replace("a", "z", "X").is_err());
        assert!(apply_str_replace("a", "", "X").is_err());
        assert_eq!(apply_insert("a\nb\n", 0, "N").unwrap(), "N\na\nb\n");
        assert_eq!(apply_insert("a\nb\n", 1, "N").unwrap(), "a\nN\nb\n");
        assert_eq!(apply_insert("a\nb", 2, "N").unwrap(), "a\nb\nN\n");
        assert!(apply_insert("a\n", 5, "N").is_err());
    }
}
