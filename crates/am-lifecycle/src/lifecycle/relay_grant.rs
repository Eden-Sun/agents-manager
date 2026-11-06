//! 專家模式執行者授權在「turn 變成可送出」那一句 SQL 裡的核對（Astra review3 #1／#2）。
//!
//! 授權的真相投影在 `projects.executor_grants_json`（daemon 的 `executor_grants.rs`）。派工核權（`relay_policy`）跟
//! 建 turn 之間沒有鎖；改成讓 turn 進入 in_flight 的兩個寫入——直接送出時的 `INSERT … 'in_flight'`（`prompt.rs`）與
//! 排隊認領的 `claim_queued`（`turn_controller.rs`，queued→in_flight 唯一的一條邊）——在同一句裡附上這段條件。
//! SQLite 的寫入彼此序列化：撤銷（任何讓授權消失的投影：PUT、改角色／track、刪除、離開 expert）若先提交，這一句就
//! 寫不進去（直接送出回可重試的 409、排隊的留在佇列等 flush 重驗撤回）；這一句先提交，就是「已開始」，不中止。
//! 不靠鎖、沒有逾時，也就沒有 fail-open。
//!
//! 只管「需要授權」的那一種派工：同一個 expert 專案裡，頂層 specialist 送給頂層 executor、而且送件者不是本專案的
//! 副管理者（代班中的權限是 runtime 狀態，照舊由 `relay_policy` 判斷）。其他組合（manager、advisor、跨專案、
//! daemon 通知、使用者）這段條件一律成立，行為不變。
//!
//! 授權成立＝這顆執行者的**第一筆**授權（排他）就是這位專家，且 track 等於專家現在的 track（同 `relay_policy::granted`）。

/// 一段可放進 `WHERE` 的 SQL 布林式：`from`／`to` 是 SQL 運算式（`?`、欄位名都行），`from` 為 NULL 時成立。
pub fn grant_allows_sql(from: &str, to: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM bots s JOIN bots x ON x.project_id = s.project_id JOIN projects p ON p.id = s.project_id
           WHERE s.id = {from} AND x.id = {to} AND p.mode = 'expert'
             AND s.role = 'specialist' AND x.role = 'executor' AND s.parent_bot_id IS NULL AND x.parent_bot_id IS NULL
             AND (p.deputy_bot_id IS NULL OR p.deputy_bot_id <> s.id)
             AND NOT EXISTS (SELECT 1 FROM (SELECT json_extract(g.value, '$.specialist') AS sp, json_extract(g.value, '$.track') AS tr
                                              FROM json_each(p.executor_grants_json) g
                                             WHERE json_extract(g.value, '$.executor') = x.id ORDER BY CAST(g.key AS INTEGER) LIMIT 1) first
                              WHERE first.sp = s.id AND first.tr = s.track))"
    )
}

/// 排隊中的 `turns.{turn}`：裡面每一則 bot 派工（`queue_class='relay'`、已驗證的 `relay_from`）都還被允許。
pub fn queued_turn_grants_ok_sql(turn: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM turns gt JOIN conversations gc ON gc.id = gt.conversation_id JOIN messages gm ON gm.turn_id = gt.id
           WHERE gt.id = {turn} AND gt.queue_class = 'relay' AND gm.role = 'user' AND gm.relay_from IS NOT NULL AND gm.relay_unverified = 0
             AND NOT ({}))",
        grant_allows_sql("gm.relay_from", "gc.bot_id")
    )
}
