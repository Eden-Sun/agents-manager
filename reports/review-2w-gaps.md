# 兩週審查未覆蓋區域補審（issue #2214）

審查範圍：唯讀，不改程式。標 ✅ 已逐段讀 diff；標 ⚠ 只抽樣；標 ✗ 未讀。嚴重度：高／中／低；信心：高／中／低。

## 1. scripts/ops/daemon-swap.sh（✅ 四個 sha 都讀完）

| sha | 檔:行 | 結論 |
|---|---|---|
| 8c59faffe | daemon-swap.sh ~1007–1018 | stop_daemon 前續租 restart lease，失敗 exit 4 並釋放窗口。看不出問題。 |
| 93ac0fb18 | daemon-swap.sh `deleted_on_purpose` 的 SQL | **低／中**：新增 `i.status IN ('pending','running')` 不設時間下界。舊的未完成 delete intent（建立於 SWAP_T0 之前、一直 pending）只要同一 bot 在窗口內以別的路徑被 `deleted_at` 標記，就會被當成刻意刪除、不回滾。觸發情境罕見（需要卡住的 intent 加上窗口內的 deleted_at），但判準比注釋寫的寬。建議：把 pending/running 限定在 `created_at >= SWAP_T0` 或 `updated_at >= SWAP_T0`，或在注釋寫明這是刻意的。 |
| 9e308e011 | `write_rejected_marker`、rollback 兩處 | 寫入順序與原子 rename 正確。寫入失敗只記 WARN，依 kick 補寫，符合設計。看不出問題。 |
| 90d45ff51 | `write_built_marker`、EXIT 11 | 寫 `.built` 失敗時 daemon 已健康在線，改 exit 11 並跳過 prune，只影響清理，不影響已換版的 daemon。看不出問題。 |

## 2. tui_prompts.rs 的 claude 2.1.326／2.1.331／2.1.332 畫面判讀（⚠ 三個 sha 的 tui_prompts 部分）

- **736b8f90b（2.1.332 held message）✅**：撤回合成的 `Always deliver` 判讀，改用實錄。游標計數、`MENU_BELOW_LINES` 規則與兩個真畫面 fixture 的測試一致。看不出問題。
- **68eb5cc55 `auto_mode_offer_keys` ✅**：游標不在 No 時送 `Down Enter`。游標在 Yes 時正確。**低／低信心**：`cursor_text` 找不到游標（畫面上沒有 `❯`）時也走 `Down Enter`；若畫面實際停在 No 而游標看不見，Down 會回到 Yes，Enter 就選到「set auto mode」，正是注釋說要避開的情況。觸發需要游標不可見，目前沒有實錄證明會發生；建議補一張游標不可見的真畫面，確認游標不可見的情況不存在，否則改成找不到游標就不送鍵。
- **68eb5cc55 `has_prompt_cue` 的 PageUp/PageDown 規則 ✅**：`norm.contains("pageup") && norm.contains("pagedown")` 只要同一行出現兩個鍵名就算作答提示。**低**：一段正文「PageUp and PageDown to scroll the docs」會被認成作答提示，`screen_has_answerable_prompt` 因此回 `true`。這跟函式「寧可說有」的設計方向一致，影響只是該 pane 多等一輪；測試只擋了單一鍵名。codex 的捲動提示字樣是合成的（注釋自己承認，未經實機），這點要留意。
- **d143c2bc3（2.1.326 畫面）⚠**：只看了 tui_prompts.rs 的 105 行新增，未逐條對 fixture。未發現明確問題。

## 3. api_bot 的 fix（⚠ 抽樣 3 個，✗ 其餘未讀）

- **1801dad13 單回合成本上限可留空 ✅**：`None` 改成 `i64::MAX`，0 仍視為無效。注釋寫明是使用者 2026-11-19 的裁示，所以不算缺陷。但每回合上限不再擋任何東西，費用控制只剩別處的總上限。**請確認**總上限（日／月或每 bot）確實存在並會擋住，否則開一個沒設上限的 bot 沒有任何煞車。
- **32036abb1 續接先回 ActiveRun ✅**：移到 `start_body_inner`，條件 `Mode::Resume { required: false }` 與舊的 `!cleared && !required` 等價，行為順序照預期。**低／中信心**：新順序在 `gate_not_ready_or_quarantined` 之前。已有 active run 的 bot 若被 quarantine，現在會回 `ActiveRun`，呼叫端會去接那顆 run，不再回 `Unconfirmed`。若 quarantine 的意思是「這個 bot 不該再被接」，這是回歸。建議：確認呼叫端接到 ActiveRun 的行為在 quarantine 下可以接受，或把 quarantine 檢查放回前面。
- **2c1ac676b StaleFence／StaleRevision ✅（部分）**：typed error 取代字串比對，錯誤文字維持一致。`SaveDiag` 只放 id 與數字，不放 prompt 或金鑰，符合注釋。未讀完 journal 的 2c1ac676b 其餘部分。
- ✗ 其餘約 14 個 api_bot fix 未讀（列在 `git log --grep api.bot` 的結果裡）。

## 4. 網頁 98 個 commit（✗ 未讀）

近 3 週 `web/` 共 330 個 commit，範圍內 98 個沒有逐一對照，也沒有做 UI 截圖。這一段**未完成**，需要另派（建議分 2–3 批，各看一段 `git log` 的使用者可見改動）。

## 結論

- 有具體觸發情境、可立即處理的：無高嚴重度項目。
- 需要使用者確認的：1801dad13 的總上限是否真的存在（1 項）；32036abb1 在 quarantine 下回 ActiveRun 是否可接受（1 項）。
- 候選（低）：93ac0fb18 SQL 判準寬、68eb5cc55 游標不可見時的 Down、has_prompt_cue 的 PageUp/PageDown 寬鬆規則。
- 未覆蓋：web 98 個 commit、api_bot 其餘約 14 個 fix、d143c2bc3 與 2c1ac676b 的完整 diff。本報告**不完整**，不應視為 #2214 的完成版。
