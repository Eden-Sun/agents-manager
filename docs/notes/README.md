# 模組線與 Codex Session 血統（Lineage）

本目錄包含 Agents Manager 專案的模組線定義與長期血統記憶筆記（issue #1943）。

## 模組線定義檔 (`lines.toml`)

- 檔名：`docs/notes/lines.toml`
- 格式：TOML，包含 `[lines.<線名>]` 表格與 `paths = ["<glob>", ...]` 清單。
- 線名規則：1–64 個字元，由 `[A-Za-z0-9._-]` 組成，且必須以英數開頭。
- 用途：
  - `agm lineage pick <path>...`：依變更檔案路徑自動挑選單一模組線（跨線或未匹配則回傳 null，指示呼叫端走一般 fork）。
  - `agm lineage since <line>`：取該線目前檢查點的 HEAD SHA，比對自檢查點以來該線路徑的 `git diff --stat`，並產生可直接注入派工 prompt 前綴的中文說明。
  - `agm lineage dispatch`：把血統派工串成單一指令（`pick → since → claim → 啟動 worker → 前綴注入 → 派送 prompt`）。
  - `agm lineage finish`：結束認領（`--merged` 帶 `claim_id` 前移檢查點；`--abandon` 放棄並回傳要接手的備份 session）。
  - `agm consult`：對 `role=thinker` 的檢查點（預設 `lead`）做唯讀 ephemeral fork 問一題（`-s read-only`、最新 `sol`＋`xhigh`），不登記、不 claim、不前移。

## Thinker 與 OB 的分工（C）

- **OB（`scripts/ob.py`）**：外部 ChatGPT 網頁，看不到本機 repo、單一共用 worker、走瀏覽器、無法接進派工 → 適合「跨出 repo、純問一題給第二意見」。
- **Thinker（`lead` 線＋`agm consult`）**：本機 repo 的唯讀 lead 血統（在 `docs/notes/lines.toml` 定義 `[lines.lead]`、`role = "thinker"`、`paths = []`，讓 `since` 比對整個 repo），以 `codex exec -s read-only --json --ephemeral fork <checkpoint> -` 命中父 session 快取、不落 rollout、不改檔、不開子 agent；適合需要完整 SPEC／決策史／各線筆記脈絡的任務拆解（plan 與驗收條件）、diff review、或重種前整理模組筆記。
- **種下 thinker 檢查點**：`agm lineage advance lead <session_id> --role thinker --head <sha>`。
- **使用範例**：`agm consult "這張票該拆成哪幾條線？"` 或 `agm consult --prompt-file plan_q.md`（可用 `--model`／`--effort`／`--line` 覆寫）。

## 血統派工與收尾流程（B3）

1. **小、單寫者（`resume` 模式）**：
   - `agm lineage dispatch --paths <files...> --prompt-file <file> (--bot <id> | --child <name>)`
   - 先跑無 prompt 的 `codex exec fork --json <checkpoint>` 產生乾淨備份 `backup_session_id`（不呼叫模型）。
   - 向 daemon 認領 `mode=resume`（帶 `backup_session_id`）；若該線已有 open resume 認領（409 `lineage_line_busy`），自動改走下方 `fork` 模式。
   - 啟動 worker 直接接回檢查點（專家模式：`POST /api/bots/{id}/start|restart?resume=native&session=<checkpoint>`；母子模式：`herdr agent start <child> --kind codex --pane <pane> -- resume <checkpoint>`），再於 prompt 前加上 `agm lineage since <line>` 的 `prompt_prefix` 送出。
   - 成功合流後執行 `agm lineage finish <claim_id> --merged --head <sha> --summary "…"`（帶 `claim_id` 前移）；失敗則執行 `agm lineage finish <claim_id> --abandon`，用回傳的 `backup_session_id` 接手。
2. **大／跨線／平行（`--fork` 或 busy 改 fork）**：
   - 認領 `mode=fork`（不擋線），worker 從檢查點持久分岔（專家模式：`?resume=fork&session=<checkpoint>`；母子模式：`herdr agent start … -- fork <checkpoint>`）。
   - 取得新產生的 `work_session_id` 後，向 daemon 登記為 `active` session 並寫回 claim（`PATCH /api/projects/{id}/lineage/claim/{claim_id}`），再送出帶 `since` 前綴的 prompt。
   - 成功合流後同樣以 `agm lineage finish <claim_id> --merged …` 前移；若檢查點已被別人移走或 claim 已關閉（409 `lineage_checkpoint_moved`／`lineage_claim_closed`），原樣回報給呼叫端，不重試也不改用無 claim 的 advance。
3. **失敗自動回滾**：`dispatch` 在認領成立後若啟動 worker、登記或送 prompt 任一步失敗，會自動呼叫 `abandon` 釋放 claim（母子模式並關閉新建的 split pane），再以非 0 輸出錯誤 JSON。

## 目前定義的模組線

1. **`web`**：前端網頁介面（`web/**`）。
2. **`lifecycle`**：Agent 生命週期管理、狀態機、TUI 解析與測試（`crates/am-lifecycle/**`、`daemon/src/lifecycle_tests/**`）。
3. **`quota`**：額度計算、用量監控與重置（`crates/am-base/src/quota*`、`crates/am-ports/src/quota*`、`daemon/src/runners/quota*` 等）。
4. **`ops`**：自動化腳本、CI、部署與系統維護（`scripts/**`、`.github/**`、`ops/**`）。
