//! 把「這顆 binary 是哪個 commit 建出來的」編進去。
//!
//! 正式 daemon 是 `target/release/agents-managerd`，跟 repo 分開活著：光看 origin/main 說不出
//! 「現在跑的是哪一版、什麼時候上線的」。AGM 的重建判斷與前端的申請計數都要這個事實
//! （`GET /api/supervisor` 的 `last_deploy`，SPEC §18.2／§18.15）。
//!
//! 拿不到 git（打包來源、tarball）就寫 `unknown`——寧可說不知道，也不要編一個假的 sha。
use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AM_BUILD_SHA={sha}");
    // HEAD 換了就重編（不然 sha 會停在第一次編譯那個）。
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads");
}
