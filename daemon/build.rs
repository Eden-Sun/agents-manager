//! 把「這顆 binary 是哪個 commit 建出來的」編進去。
//!
//! 正式 daemon 是 `target/release/agents-managerd`，跟 repo 分開活著：光看 origin/main 說不出
//! 「現在跑的是哪一版、什麼時候上線的」。AGM 的重建判斷與前端的申請計數都要這個事實
//! （`GET /api/supervisor` 的 `last_deploy`，SPEC §18.2／§18.15）。
//!
//! 拿不到 git（打包來源、tarball）就寫 `unknown`——寧可說不知道，也不要編一個假的 sha。
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

fn main() {
    // 完整 sha：`--version` 印它，daemon-swap 換 binary 之前拿它跟核准的 sha 比（`build_info::VERSION`）。
    let full = git(&["rev-parse", "HEAD"]).filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".to_string());
    let short = git(&["rev-parse", "--short", "HEAD"]).filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".to_string());
    // 髒樹（有未提交的改動）建出來的 binary 不是任何一個 commit：標記出來，不能冒充乾淨的 sha。
    // 只看追蹤中的檔案（`-uno`）：沒追蹤的檔案進不了 binary。拿不到 git 就不標（sha 本來就是 unknown）。
    let dirty = git(&["status", "--porcelain", "-uno"]).is_some_and(|s| !s.is_empty());
    println!("cargo:rustc-env=AM_BUILD_SHA={short}");
    println!("cargo:rustc-env=AM_BUILD_SHA_FULL={full}");
    println!("cargo:rustc-env=AM_BUILD_DIRTY={}", if dirty { "1" } else { "0" });
    println!("cargo:rustc-env=AM_BUILD_DIRTY_SUFFIX={}", if dirty { "-dirty" } else { "" });
    // HEAD 換了就重編（不然 sha 會停在第一次編譯那個）；原始碼動了也重跑（不然 dirty 會停在上一次）。
    // 用 `git rev-parse --git-path`：worktree 裡 `.git` 是檔案，寫死 `../.git/HEAD` 指到不存在的路徑。
    for p in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
}
