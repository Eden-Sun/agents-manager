#![allow(dead_code, unused_imports)]
use super::*;
use super::*;
use std::time::{Duration, Instant};




#[tokio::test]
async fn remote_spool_scanner_exits_on_shutdown_instead_of_waiting_for_its_next_poll() {
    let env = crate::testing::env().await;
    spawn_spool_scanner(env.app.clone());
    env.app.shutdown.cancel();
    env.app.background_tasks.close();
    tokio::time::timeout(std::time::Duration::from_secs(1), env.app.background_tasks.wait())
        .await
        .expect("scanner should join promptly on daemon shutdown");
}

#[tokio::test]
async fn remote_spool_retry_joins_on_shutdown_while_waiting_to_retry() {
    let env = crate::testing::env().await;
    sqlx::query("DROP TABLE bots").execute(&env.app.db).await.unwrap();
    replay_host(&env.app, "remote").await;
    assert_eq!(env.app.background_tasks.len(), 1, "the failed pass should schedule one retry task");

    env.app.shutdown.cancel();
    env.app.background_tasks.close();
    tokio::time::timeout(std::time::Duration::from_secs(1), env.app.background_tasks.wait())
        .await
        .expect("spool retry must leave its delay on shutdown");
}

#[test]
fn the_drain_scripts_reject_unsafe_ids() {
    for id in ["../..", "x/y", r"..\..", "", "x\";id"] {
        assert!(claim_script(id, crate::startup::REMOTE_ROOT).is_err(), "unsafe id was accepted: {id:?}");
        assert!(ack_script(id, crate::startup::REMOTE_ROOT).is_err(), "unsafe id was accepted: {id:?}");
    }
}

/// issue #70 的核心不變式，釘在腳本這一層：**claim 不准刪 spool**。
/// 遠端那份是唯一的副本，刪它的唯一時機是本機 commit 之後（`ack_script`）。
#[test]
fn the_claim_script_never_deletes_the_spool_it_just_read() {
    let s = claim_script("botX", crate::startup::REMOTE_ROOT).unwrap();
    assert!(s.contains("cat \"$f.replaying\""), "要讀出來：{s}");
    assert!(!s.contains("rm -f \"$f.replaying\""), "claim 階段不可以刪 .replaying：{s}");
    assert!(s.contains("rm -f \"$s\""), "status 仍是讀完就刪：{s}");

    let ack = ack_script("botX", crate::startup::REMOTE_ROOT).unwrap();
    assert!(ack.contains("rm -f \"$d/hook-spool.jsonl.replaying\""), "ack 才刪：{ack}");
    assert!(!ack.contains("cat "), "ack 不再讀任何東西：{ack}");
}

#[test]
fn the_claim_script_takes_the_spool_and_the_status_slot() {
    let s = claim_script("botX", crate::startup::REMOTE_ROOT).unwrap();
    assert!(s.contains("bots/\"'botX'"));
    assert!(s.contains("\"$HOME/.config/agents-manager/bots/\""), "正式實例路徑不變：{s}");
    let iso = crate::startup::remote_root_for(Some("a1b2"));
    assert!(claim_script("botX", &iso).unwrap().contains("\"$HOME/.config/agents-manager/instances/a1b2/bots/\""));
    assert!(scan_script(crate::startup::REMOTE_ROOT).contains("\"$HOME/.config/agents-manager/bots\"/*/"));
    assert!(scan_script(&iso).contains("\"$HOME/.config/agents-manager/instances/a1b2/bots\"/*/"));
    assert_eq!(crate::startup::remote_root_for(None), crate::startup::REMOTE_ROOT);
    assert_eq!(crate::startup::remote_root_for(Some("a1b2")), ".config/agents-manager/instances/a1b2");
    assert!(s.contains("mv \"$f\" \"$f.claim\""), "要先 rename 再讀：{s}");
    assert!(!s.contains("rm -f \"$f\";"), "claim 階段不准刪 live spool：{s}");
    assert!(s.contains("hook-status.json"));
    assert!(s.contains(STATUS_MARKER));
}
