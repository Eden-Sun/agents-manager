//! 專家模式派工的依據，在「turn 變成可送出」那一句 SQL 裡確認（Astra review3／review4）。
//!
//! 專家派工的開關投影在 `projects.specialist_dispatch`（config 的 `ProjectCfg.specialist_dispatch`，daemon 的
//! `specialist_dispatch.rs`）。派工核權（daemon `relay_policy`）跟建 turn 之間沒有鎖；改成讓 turn 進入 in_flight 的兩個寫入——
//! 直接送出時的 `INSERT … 'in_flight'`（`prompt.rs`）與排隊認領 `claim_queued`（`turn_controller.rs`，queued→in_flight 唯一的一條邊）——
//! 在同一句裡附上這段條件。SQLite 的寫入彼此序列化：關掉開關（或降級等）若先提交，這一句就寫不進去（直接送出回可重試的 409、
//! 排隊的留在佇列等 flush 重驗撤回）；這一句先提交，就是「已開始」，不中止。不靠鎖、沒有逾時，也就沒有 fail-open。
//!
//! 依據（`messages.relay_auth`，核權時由 daemon 記下，見 `RelaySrc::auth`；成立的依據都記、逗號分隔，**任一**仍成立就放行）決定怎麼確認：
//! - `specialist:<專案 id>`：**正向**確認送件者此刻仍是該專案存活頂層 specialist、目標仍是該專案存活頂層 executor、專案仍是
//!   expert（未刪、未移交）且 `specialist_dispatch` 仍開著。降級、變 child、搬專案、刪除、關掉開關任何一項都不成立。
//! - `deputy:<專案 id>`：正向確認送件者此刻仍「代班中」（同 daemon `relay_deputy::acting`：是該 expert 專案的副管理者，
//!   且名下有改派來、還沒做完的請求）。只是被指定、沒在代班，不成立。
//! - 沒有依據（原本就不靠開關放行的組合：manager、advisor 目標、直屬 parent…）：只擋「同一 expert 專案內頂層
//!   specialist→頂層 executor」這一種需要依據的組合（核權後才被改成 specialist 的送件者），其他照舊放行。
//! - 清單裡沒有任何一項成立（含無法辨識的值，例如舊版的 `grant:<專案>`）＝不成立。
//! - 舊版（SCHEMA 82～83）逐對授權記下的 `grant:` 依據不再認：已排隊的舊派工認領不到、flush 撤回並通知送件者重送。

/// 一段 SQL 布林式；`from`／`to`／`auth` 是 SQL 運算式（欄位名，可重複引用）。`from` 為 NULL（不是 bot 派工）時成立。
pub fn relay_allowed_sql(from: &str, to: &str, auth: &str) -> String {
    // `auth` 是逗號分隔的依據清單；某一項是否列在裡面：instr(',' || auth || ',', ',<項>,') > 0。
    let has = |item: &str| format!("instr(',' || {auth} || ',', ',' || {item} || ',') > 0");
    let has_specialist = has("'specialist:' || p.id");
    let has_deputy = has("'deputy:' || p.id");
    format!(
        "(CASE
           WHEN {from} IS NULL THEN 1
           WHEN {auth} IS NULL THEN NOT EXISTS (SELECT 1 FROM bots s JOIN bots x ON x.project_id = s.project_id JOIN projects p ON p.id = s.project_id
                 WHERE s.id = {from} AND x.id = {to} AND p.mode = 'expert'
                   AND s.role = 'specialist' AND x.role = 'executor' AND s.parent_bot_id IS NULL AND x.parent_bot_id IS NULL)
           ELSE EXISTS (SELECT 1 FROM bots s JOIN bots x ON x.project_id = s.project_id JOIN projects p ON p.id = s.project_id
                 WHERE s.id = {from} AND x.id = {to} AND {has_specialist}
                   AND p.mode = 'expert' AND p.specialist_dispatch = 1 AND p.deleted_at IS NULL AND p.handed_off_to IS NULL
                   AND s.deleted_at IS NULL AND x.deleted_at IS NULL AND s.parent_bot_id IS NULL AND x.parent_bot_id IS NULL
                   AND s.role = 'specialist' AND x.role = 'executor')
             OR EXISTS (SELECT 1 FROM bots s JOIN projects p ON p.id = s.project_id JOIN bots x ON x.project_id = p.id
                 WHERE s.id = {from} AND x.id = {to} AND {has_deputy}
                   AND p.mode = 'expert' AND p.deleted_at IS NULL AND p.handed_off_to IS NULL AND p.deputy_bot_id = s.id
                   AND s.deleted_at IS NULL AND x.deleted_at IS NULL
                   AND EXISTS (SELECT 1 FROM relay_requests r WHERE r.to_bot_id = s.id AND r.redirected_from IS NOT NULL AND r.state IN ('pending','delivered')))
         END)"
    )
}

/// 同 [`relay_allowed_sql`]，但三個值用 bind（依序 `from`、`to`、`auth` 三個 `?`），每個只 bind 一次。
pub fn relay_allowed_params_sql() -> String {
    format!("(SELECT {} FROM (SELECT ? AS f, ? AS t, ? AS a) ra)", relay_allowed_sql("ra.f", "ra.t", "ra.a"))
}

/// 排隊中的 `turns.{turn}`：裡面每一則 bot 派工（`queue_class='relay'`、已驗證的 `relay_from`）都還被允許。
pub fn queued_turn_grants_ok_sql(turn: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM turns gt JOIN conversations gc ON gc.id = gt.conversation_id JOIN messages gm ON gm.turn_id = gt.id
           WHERE gt.id = {turn} AND gt.queue_class = 'relay' AND gm.role = 'user' AND gm.relay_from IS NOT NULL AND gm.relay_unverified = 0
             AND NOT {})",
        relay_allowed_sql("gm.relay_from", "gc.bot_id", "gm.relay_auth")
    )
}
