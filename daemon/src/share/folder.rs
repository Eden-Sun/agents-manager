//! 受限 bot 的資料夾（使用者 2026-10-03 裁示）：以資料夾為單位，建 bot 時選一個資料夾，那就是它能碰的全部範圍（cwd＝它）。
//!
//! - **新資料夾**：給名字，建在固定的根目錄 `[share] folders_root`（預設 `~/shared-bots`）底下，**在 daemon 資料目錄之外**。
//! - **既有資料夾**：本機任意路徑（例如某個專案目錄）。裡面所有檔案（含 `.env` 這類）end user 都可能透過 bot 讀到，UI 會講清楚；
//!   這裡擋掉一選就會把機器上的秘密整包交出去的位置：根目錄、家目錄本身與它的祖先、daemon 資料目錄、`~/.ssh`、`~/.config`、
//!   帳號目錄（`~/.claude*`）、系統目錄。
//! - 上傳的檔一律放 `<資料夾>/inbox/`（不存在就建）。
//! - 資料夾裡的 `CLAUDE.md`／`.claude/CLAUDE.md`／`AGENTS.md` 與 `memory/*.md`、`.claude/memory/*.md` 由 daemon 讀進系統提示
//!   （[`instructions`]）：`--restricted` 不讀 project／user settings，claude 也就不會自己載入資料夾的 CLAUDE.md，
//!   auto-memory 在 `--restricted` 下也整個關掉（2026-10-03 對 claude 2.1.288 實測）。

use std::ffi::OsStr;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::lifecycle::LcError;

/// `POST /api/projects/{id}/bots` 的 `share_folder`。沒帶＝新資料夾、名字用 bot 名。
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShareFolderIn {
    New { name: String },
    Existing { path: String },
}

/// 新資料夾的根目錄：`[share] folders_root`（可用 `~/`），沒設＝`~/shared-bots`。
pub(crate) fn root(cfg_root: Option<&str>, home: &str) -> PathBuf {
    match cfg_root.map(str::trim).filter(|s| !s.is_empty()) {
        Some(r) => PathBuf::from(crate::config::expand_home(r, home)),
        None => Path::new(home).join("shared-bots"),
    }
}

fn bad(message: impl Into<String>) -> LcError {
    LcError::BadValue(json!({"error": "bad_request", "reason": "bad_share_folder", "message": message.into()}))
}

/// 新資料夾的名字：英數開頭，`[A-Za-z0-9._-]`，最多 64 字；不能是 `.`／`..` 或藏起來的名字。
pub(crate) fn check_new_name(name: &str) -> Result<(), LcError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(bad("資料夾名稱只能用英數與 . _ -，英數開頭，最多 64 字"))
    }
}

/// 一選下去就把機器上的秘密整包交出去的位置。`folder`、`home`、`data_dir` 都要是 canonicalize 過的。
pub(crate) fn unsafe_reason(folder: &Path, home: &Path, data_dir: &Path) -> Option<&'static str> {
    if folder.parent().is_none() {
        return Some("不能分享整個根目錄");
    }
    if home.starts_with(folder) {
        return Some("不能分享家目錄本身或它的上層");
    }
    if data_dir.starts_with(folder) || folder.starts_with(data_dir) {
        return Some("不能分享 AG Man 的資料目錄（ui-token、config、DB 都在那裡）");
    }
    if let Ok(rest) = folder.strip_prefix(home) {
        if let Some(Component::Normal(first)) = rest.components().next() {
            let f = first.to_string_lossy();
            if f.starts_with(".claude")
                || [".ssh", ".gnupg", ".aws", ".config", ".codex", ".grok", ".docker", ".kube", ".local", ".cargo", ".npm"].contains(&f.as_ref())
            {
                return Some("不能分享帳號、金鑰或設定目錄（~/.ssh、~/.claude*、~/.config…）");
            }
        }
    }
    const SYSTEM: &[&str] = &["/etc", "/proc", "/sys", "/dev", "/boot", "/root", "/run", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/snap", "/System", "/Library", "/private/etc"];
    if SYSTEM.iter().any(|s| folder.starts_with(s)) {
        return Some("不能分享系統目錄");
    }
    None
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// 既有資料夾：絕對路徑、存在、是目錄、不在 [`unsafe_reason`] 的範圍。回 canonicalize 過的路徑（符號連結解開，存的是真正的位置）。
pub(crate) fn check_existing(path: &str, home: &Path, data_dir: &Path) -> Result<PathBuf, LcError> {
    let p = Path::new(path.trim());
    if !p.is_absolute() {
        return Err(bad("既有資料夾要給絕對路徑"));
    }
    let real = std::fs::canonicalize(p).map_err(|_| bad("找不到這個資料夾"))?;
    if !real.is_dir() {
        return Err(bad("這個路徑不是資料夾"));
    }
    if let Some(why) = unsafe_reason(&real, &canonical(home), &canonical(data_dir)) {
        return Err(bad(why));
    }
    Ok(real)
}

/// 建新資料夾（0700）。已經有同名的就 409 `folder_exists`：不能讓「新資料夾」悄悄變成分享一個既有的。
pub(crate) fn create_new(root: &Path, name: &str, home: &Path, data_dir: &Path) -> Result<PathBuf, LcError> {
    check_new_name(name)?;
    std::fs::create_dir_all(root).map_err(|e| LcError::Upstream(format!("share folders root {}: {e}", root.display())))?;
    let root = canonical(root);
    if let Some(why) = unsafe_reason(&root, &canonical(home), &canonical(data_dir)) {
        return Err(bad(format!("[share] folders_root 的位置不行：{why}")));
    }
    let dir = root.join(name);
    let mut b = std::fs::DirBuilder::new();
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    match b.create(&dir) {
        Ok(()) => Ok(dir),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(LcError::conflict(
            "folder_exists",
            json!({"path": dir.to_string_lossy(), "message": "這個名字的資料夾已經有了；要用它請選「既有資料夾」，不然換個名字"}),
        )),
        Err(e) => Err(LcError::Upstream(format!("create share folder {}: {e}", dir.display()))),
    }
}

/// `<資料夾>/inbox/`（不存在就建，0700，不跟符號連結）。資料夾本身不見了就失敗：不替使用者重建一個空的。
pub(crate) fn ensure_inbox(folder: &Path) -> std::io::Result<std::fs::File> {
    if !folder.is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "share folder is missing"));
    }
    crate::trusted_open::create_private_bound_dirs(folder, &[OsStr::new("inbox")])
}

/// 每個檔最多讀這麼多；整段指示最多這麼多（超過的檔略過並註明）。
const MD_MAX: u64 = 32 * 1024;
const TOTAL_MAX: usize = 96 * 1024;

fn read_bound(folder: &Path, parts: &[&str]) -> Option<String> {
    let comps: Vec<&OsStr> = parts.iter().map(OsStr::new).collect();
    let f = crate::trusted_open::open_bound_file(folder, &comps, None).ok()?;
    let bytes = crate::trusted_open::read_limited(f, MD_MAX).ok()?;
    String::from_utf8(bytes).ok().filter(|s| !s.trim().is_empty())
}

/// 資料夾的指示與記憶，給系統提示用。全部 `O_NOFOLLOW` 逐層打開：既有資料夾裡的 `CLAUDE.md` 若是指到外面
/// （例如 ui-token）的符號連結，一律不讀——不然 daemon 會替 bot 把資料夾外的東西讀進它的提示。
pub(crate) fn instructions(folder: &Path) -> String {
    let mut out = String::new();
    let push = |label: &str, text: &str, out: &mut String| {
        if out.len() + text.len() > TOTAL_MAX {
            out.push_str(&format!("\n（{label} 太大，沒有載入）\n"));
            return;
        }
        out.push_str(&format!("\n\n## {label}\n\n{}\n", text.trim_end()));
    };
    for parts in [&["CLAUDE.md"][..], &[".claude", "CLAUDE.md"], &["AGENTS.md"]] {
        if let Some(t) = read_bound(folder, parts) {
            push(&format!("資料夾的指示：{}", parts.join("/")), &t, &mut out);
        }
    }
    // `memory/` 是 bot 自己寫的地方；`.claude/memory/`（claude auto-memory 的格式，既有資料夾可能已經有）唯讀照樣載入——
    // claude 不准受限 bot 寫 `.claude/` 底下（明確 allow 也一樣，2.1.288 實測），所以新記憶不能放那裡。
    for mem in [&["memory"][..], &[".claude", "memory"]] {
        let label = mem.join("/");
        let mem: Vec<&OsStr> = mem.iter().map(OsStr::new).collect();
        if let Ok(dir) = crate::trusted_open::open_bound_dir(folder, &mem, None) {
            let mut names: Vec<String> = crate::trusted_open::read_dir_bound(&dir)
                .unwrap_or_default()
                .into_iter()
                .filter(|e| e.is_file)
                .map(|e| e.name.to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".md") && !n.starts_with('.'))
                .collect();
            // 索引在前，其餘照名字。
            names.sort_by_key(|n| (n != "MEMORY.md", n.clone()));
            for n in names.into_iter().take(64) {
                let Ok(f) = crate::trusted_open::open_entry_in(&dir, OsStr::new(&n)) else { continue };
                let mut s = String::new();
                if f.take(MD_MAX).read_to_string(&mut s).is_ok() && !s.trim().is_empty() {
                    push(&format!("記憶：{label}/{n}"), &s, &mut out);
                }
            }
        }
    }
    out
}

/// 驗證受限 bot 的 workspace 路徑是否合法（issue #828）。
/// 必須是絕對路徑、不是根目錄的一級子目錄、通過 `unsafe_reason`、且原地若是 symlink 則拒絕。
pub(crate) fn validate_workspace_path(data_dir: &Path, workspace: &str) -> Option<PathBuf> {
    let p = Path::new(workspace.trim());
    if !p.is_absolute() {
        return None;
    }
    let parent = p.parent()?;
    if parent.parent().is_none() {
        return None;
    }
    let home_str = crate::share::cage::local_home();
    let home = Path::new(&home_str);
    if unsafe_reason(p, home, data_dir).is_some() {
        return None;
    }
    if let Ok(meta) = std::fs::symlink_metadata(p) {
        if meta.file_type().is_symlink() {
            return None;
        }
    }
    Some(p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_folders_are_refused() {
        let home = Path::new("/home/u");
        let data = Path::new("/home/u/.config/agents-manager");
        for (p, bad) in [
            ("/", true),
            ("/home", true),
            ("/home/u", true),
            ("/home/u/.config", true),
            ("/home/u/.config/agents-manager/outbox", true),
            ("/home/u/.ssh", true),
            ("/home/u/.claude-cc1/projects", true),
            ("/etc/nginx", true),
            ("/home/u/project/site", false),
            ("/home/u/shared-bots/support", false),
            ("/srv/docs", false),
            ("/home/u/.configs-not", false),
        ] {
            assert_eq!(unsafe_reason(Path::new(p), home, data).is_some(), bad, "{p}");
        }
    }

    #[test]
    fn new_folder_names_are_plain() {
        for ok in ["support", "kefu", "a.b_c-1"] {
            assert!(check_new_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "..", "a/b", "-x", "a b", &"x".repeat(65)] {
            assert!(check_new_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_new_folder_never_reuses_an_existing_one_and_existing_ones_resolve_symlinks() {
        let base = crate::testing::scratch_dir("am-share-folder");
        let home = base.join("home");
        let data = home.join(".config/agents-manager");
        std::fs::create_dir_all(&data).unwrap();
        let root = home.join("shared-bots");
        let made = create_new(&root, "support", &home, &data).unwrap();
        assert!(made.is_dir());
        assert!(matches!(create_new(&root, "support", &home, &data), Err(LcError::Conflict(_))), "同名 409，不悄悄共用");
        // 符號連結指到資料目錄：照真正的位置擋。
        std::os::unix::fs::symlink(&data, home.join("sneaky")).unwrap();
        assert!(check_existing(&home.join("sneaky").to_string_lossy(), &home, &data).is_err());
        assert!(check_existing("relative/path", &home, &data).is_err());
        let proj = home.join("project/site");
        std::fs::create_dir_all(&proj).unwrap();
        assert_eq!(check_existing(&proj.to_string_lossy(), &home, &data).unwrap(), std::fs::canonicalize(&proj).unwrap());
    }

    #[test]
    fn instructions_load_md_and_memory_but_never_follow_symlinks_out() {
        let f = crate::testing::scratch_dir("am-share-md");
        std::fs::write(f.join("CLAUDE.md"), "codeword PELICAN").unwrap();
        std::fs::write(f.join("AGENTS.md"), "codeword WALRUS").unwrap();
        std::fs::create_dir_all(f.join(".claude/memory")).unwrap();
        std::fs::write(f.join(".claude/memory/MEMORY.md"), "- [color](color.md)").unwrap();
        std::fs::write(f.join(".claude/memory/color.md"), "favorite color TEAL").unwrap();
        std::fs::create_dir_all(f.join("memory")).unwrap();
        std::fs::write(f.join("memory/MEMORY.md"), "- [bird](bird.md)").unwrap();
        std::fs::write(f.join("memory/bird.md"), "remember PUFFIN").unwrap();
        let outside = crate::testing::scratch_dir("am-share-secret");
        std::fs::write(outside.join("ui-token"), "SECRET-TOKEN").unwrap();
        std::fs::create_dir_all(f.join(".claude")).unwrap();
        std::os::unix::fs::symlink(outside.join("ui-token"), f.join(".claude/CLAUDE.md")).unwrap();
        std::os::unix::fs::symlink(outside.join("ui-token"), f.join(".claude/memory/leak.md")).unwrap();
        let text = instructions(&f);
        for want in ["PELICAN", "WALRUS", "TEAL", "PUFFIN", "MEMORY.md"] {
            assert!(text.contains(want), "{want} 不在：{text}");
        }
        assert!(!text.contains("SECRET-TOKEN"), "符號連結指到外面的不能讀：{text}");
        assert!(text.find("MEMORY.md").unwrap() < text.find("color.md").unwrap(), "索引在前");
    }
}
