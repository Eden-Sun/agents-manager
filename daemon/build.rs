//! 把「這顆 binary 是哪個 commit 建出來的」編進去。
//!
//! 正式 daemon 是 `target/release/agents-managerd`，跟 repo 分開活著：光看 origin/main 說不出
//! 「現在跑的是哪一版、什麼時候上線的」。AGM 的重建判斷與前端的申請計數都要這個事實
//! （`GET /api/supervisor` 的 `last_deploy`，SPEC §18.2／§18.15）。
//!
//! 拿不到 git（打包來源、tarball）就寫 `unknown`——寧可說不知道，也不要編一個假的 sha。
// 同 `crates/am-supervisor/build.rs`：release 的 build_info env 要跟著整個 workspace 重算（#884）；dev／test 不監看（#1861）。
const WORKSPACE_WATCH: &[&str] = &["../crates", "../Cargo.toml", "../Cargo.lock"];
const SHARED_SRC: &str = "../crates/am-supervisor/build_common.rs";
include!("../crates/am-supervisor/build_common.rs");
