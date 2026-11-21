
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
    status.is_none_or(|s| !s.is_empty())
}

/// 讀不到 status 一律 fail closed（#884）。例外：dev／test 在沒有 `.git` 的來源（remote-cargo 的 shared 排除了 `.git`）
/// 不標髒、也不警告（#1861）：那裡建出來的東西不會被部署，標髒只會每次都印 warning。release 仍照舊 fail closed。
fn dirty_for(status: Option<&str>, in_git: bool, release: bool) -> bool {
    if status.is_none() && !in_git && !release {
        return false;
    }
    dirty_from_status(status)
}

fn main() {
    println!("cargo:rerun-if-changed={SHARED_SRC}");
    let release = std::env::var("PROFILE").is_ok_and(|p| p == "release");
    let full = git(&["rev-parse", "HEAD"]).filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".to_string());
    let short = git(&["rev-parse", "--short", "HEAD"]).filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".to_string());
    let in_git = git(&["rev-parse", "--git-dir"]).is_some();
    let status = git(&["--no-optional-locks", "status", "--porcelain", "-uno"]);
    let dirty = dirty_for(status.as_deref(), in_git, release);
    if dirty && status.is_none() {
        println!("cargo:warning=git status failed; marking build dirty");
    }
    let changed = status.unwrap_or_default();
    if dirty {
        let files: Vec<&str> = changed.lines().take(8).collect();
        println!("cargo:warning=build tree is dirty ({} tracked file(s) changed): {}", changed.lines().count(), files.join(" | "));
    }
    println!("cargo:rustc-env=AM_BUILD_SHA={short}");
    println!("cargo:rustc-env=AM_BUILD_SHA_FULL={full}");
    println!("cargo:rustc-env=AM_BUILD_DIRTY={}", if dirty { "1" } else { "0" });
    println!("cargo:rustc-env=AM_BUILD_DIRTY_SUFFIX={}", if dirty { "-dirty" } else { "" });
    for p in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    if release {
        for p in WORKSPACE_WATCH {
            println!("cargo:rerun-if-changed={p}");
        }
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

    #[test]
    fn a_dev_build_without_git_is_not_marked_dirty() {
        assert!(!super::dirty_for(None, false, false));
    }

    #[test]
    fn a_release_build_without_git_fails_closed() {
        assert!(super::dirty_for(None, false, true));
    }

    #[test]
    fn a_failed_status_inside_a_repo_fails_closed_in_both_profiles() {
        assert!(super::dirty_for(None, true, false));
        assert!(super::dirty_for(None, true, true));
    }

    #[test]
    fn a_clean_status_is_clean_in_both_profiles() {
        assert!(!super::dirty_for(Some(""), true, false));
        assert!(!super::dirty_for(Some(""), true, true));
    }
}
