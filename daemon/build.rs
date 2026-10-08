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

fn dirty_from_status(status: Option<&str>) -> bool {
    status.map_or(true, |s| !s.is_empty())
}

fn main() {
    // 完整 sha：`--version` 印它，daemon-swap 換 binary 之前拿它跟核准的 sha 比（`build_info::VERSION`）。
    let full = git(&["rev-parse", "HEAD"]).filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".to_string());
    let short = git(&["rev-parse", "--short", "HEAD"]).filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".to_string());
    // 髒樹（有未提交的改動）建出來的 binary 不是任何一個 commit：標記出來，不能冒充乾淨的 sha。
    // 只看追蹤中的檔案（`-uno`）：沒追蹤的檔案進不了 binary。讀不到 status 就 fail closed 標髒，避免不確定的 binary 冒充乾淨 commit。
    // `--no-optional-locks`：只讀，不去刷新／寫 index（自動部署的 checkout 同時還有別的 git 指令在跑，不要跟它搶 index.lock）。
    let status = git(&["--no-optional-locks", "status", "--porcelain", "-uno"]);
    let dirty = dirty_from_status(status.as_deref());
    if status.is_none() {
        println!("cargo:warning=git status failed; marking build dirty");
    }
    let changed = status.unwrap_or_default();
    if dirty {
        // 部署會因為 `-dirty` 中止（daemon-swap.sh rc=10）：把是哪些檔案寫進 build log，才查得到是誰弄髒的。
        let files: Vec<&str> = changed.lines().take(8).collect();
        println!("cargo:warning=build tree is dirty ({} tracked file(s) changed): {}", changed.lines().count(), files.join(" | "));
    }
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
    // 同 `crates/am-supervisor/build.rs`：daemon 測試建置的 `build_info` env 來自這支，別的 crate 動了也要重跑（#884）。
    for p in ["../crates", "../Cargo.toml", "../Cargo.lock"] {
        println!("cargo:rerun-if-changed={p}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unavailable_git_status_is_not_a_clean_tree() {
        assert!(super::dirty_from_status(None), "failed status checks must fail closed");
    }

    #[test]
    fn an_empty_git_status_is_a_clean_tree() {
        assert!(!super::dirty_from_status(Some("")));
    }

    #[test]
    fn a_nonempty_git_status_is_a_dirty_tree() {
        assert!(super::dirty_from_status(Some(" M src/main.rs")));
    }
}
