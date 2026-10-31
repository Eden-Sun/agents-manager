
CREATE TABLE IF NOT EXISTS missions (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  -- 同一次「交給 AGM」重送回同一筆。
  client_request_id TEXT NOT NULL,
  text TEXT NOT NULL,
  -- D2：push_main | pr
  delivery_mode TEXT NOT NULL,
  -- D7：claude | codex | grok
  executor_kind TEXT NOT NULL,
  -- D5：wait | switch（5h 撞限時原地等，還是直接換下一個身份）
  on_5h_limit TEXT NOT NULL,
  -- review 退回＋驗證失敗合計的輪數上限；用完就停下來問人。
  max_rounds INTEGER NOT NULL DEFAULT 2,
  rounds_used INTEGER NOT NULL DEFAULT 0,
  -- 非 NULL = 停下來等人（max_rounds、no_fable_for_verifier、push_main_failed、waiting_quota…）。
  paused_reason TEXT,
  paused_detail TEXT,
  result_summary TEXT,
  -- 這一筆是哪一筆任務的續作（追加修改）。NULL = 使用者自己開的第一筆。
  -- 續作是**新的一筆 mission**，不是把舊的打開重跑：舊那筆的 completed_at、result_summary 與
  -- 事件串完全不動，使用者回頭看到的還是當初交付的那一版。
  parent_mission_id TEXT,
  -- 續作請求的正規化指紋（parent + 文字 + 選項）。同一個 project 底下兩個不同 parent 用了同一個
  -- crid 時，只比文字會把別人的續作當成自己的重放回傳。
  request_fingerprint TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  completed_at TEXT,
  cancelled_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS missions_crid ON missions(project_id, client_request_id);
CREATE INDEX IF NOT EXISTS missions_project ON missions(project_id, created_at);
CREATE TABLE IF NOT EXISTS mission_events (
  id TEXT PRIMARY KEY,
  mission_id TEXT NOT NULL,
  -- instruction | report | verified | round | paused | resumed | cancelled | delivered | completed
  -- | note | question（使用者對成果追問）| answer（回覆：使用者回答暫停，或 AGM 回覆追問）
  kind TEXT NOT NULL,
  text TEXT NOT NULL,
  -- NULL = 使用者本人；bot id = 那顆 bot（多半是 AGM）；'daemon' = daemon 自己記的。
  relay_from TEXT,
  payload_json TEXT NOT NULL DEFAULT '{}',
  -- answer 指回它回答的那則 question 事件 id，成對顯示才不會變成一串對不上的獨白。
  reply_to TEXT,
  -- 同一個請求重送回同一則事件（回答／追問都要冪等，見 api.rs 的 replay 規則）。
  client_request_id TEXT,
  -- 這個 crid 當初代表的**完整請求**（kind/text/來源/reply_to 的正規化字串）。重放只比文字是不夠的：
  -- 一則 question 與一則 answer 可以有同樣的文字，AGM 的回覆換了 reply_to 也還是同一段字。
  request_fingerprint TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS mission_events_mission ON mission_events(mission_id, created_at);
CREATE INDEX IF NOT EXISTS missions_parent ON missions(parent_mission_id);
CREATE UNIQUE INDEX IF NOT EXISTS missions_one_open_child ON missions(parent_mission_id) WHERE parent_mission_id IS NOT NULL AND completed_at IS NULL AND cancelled_at IS NULL;
CREATE INDEX IF NOT EXISTS mission_events_reply ON mission_events(reply_to);
CREATE UNIQUE INDEX IF NOT EXISTS mission_events_crid ON mission_events(mission_id, client_request_id) WHERE client_request_id IS NOT NULL;
