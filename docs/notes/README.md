# 模組線與 Codex Session 血統（Lineage）

本目錄包含 Agents Manager 專案的模組線定義與長期血統記憶筆記（issue #1943）。

## 模組線定義檔 (`lines.toml`)

- 檔名：`docs/notes/lines.toml`
- 格式：TOML，包含 `[lines.<線名>]` 表格與 `paths = ["<glob>", ...]` 清單。
- 線名規則：1–64 個字元，由 `[A-Za-z0-9._-]` 組成，且必須以英數開頭。
- 用途：
  - `agm lineage pick <path>...`：依變更檔案路徑自動挑選單一模組線（跨線或未匹配則回傳 null，指示呼叫端走一般 fork）。
  - `agm lineage since <line>`：取該線目前檢查點的 HEAD SHA，比對自檢查點以來該線路徑的 `git diff --stat`，並產生可直接注入派工 prompt 前綴的中文說明。

## 目前定義的模組線

1. **`web`**：前端網頁介面（`web/**`）。
2. **`lifecycle`**：Agent 生命週期管理、狀態機、TUI 解析與測試（`crates/am-lifecycle/**`、`daemon/src/lifecycle_tests/**`）。
3. **`quota`**：額度計算、用量監控與重置（`crates/am-base/src/quota*`、`crates/am-ports/src/quota*`、`daemon/src/runners/quota*` 等）。
4. **`ops`**：自動化腳本、CI、部署與系統維護（`scripts/**`、`.github/**`、`ops/**`）。
