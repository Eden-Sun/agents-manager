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

## 4. 網頁 commit 前半（#2214 補審）

範圍：`git log --since=2026-11-11 --reverse origin/main -- web/` 的前一半，114 個 commit（`e419f291d..e521fb229`）。唯讀。

### 覆蓋

- **機械掃描（全部看過）**：新增的原生 `alert／confirm／prompt`（無；`onConfirm` 是元件 prop）；TS／TSX 新增的中文字面量不經 `t()／L()`（66 筆，多數是註解、`msg()` 鍵與 API 錯誤對照表，實際漏的 2 筆，見下）；新增的 CSS 色碼（49 筆，其中 apiBot 三檔與時間軸已由 #2213 收斂）；`origin/main` 上 CSS 用到但未定義的自訂屬性（零筆，`ebdc1e642` 已清）。
- **抽讀**：#2213 相關的成本總覽、用量條、翻譯回顧、outbox HTML 預覽（121ba320a）、移除最近刪除頁（3205b0116）。
- **未讀**：草稿與附件（0b3b4fb8b、53a5b6783、568864c5e 系列）、排程卡片（2232c454a、ada29fd47）、窄螢幕版面的逐段 diff、其餘 Claude API 與 OpenAI 分頁的互動細節。這一半的功能面**不完整**，需要另派。

### 發現

| # | 位置 | 類別 | 觸發情境 | 嚴重度／信心 | 處理 |
|---|---|---|---|---|---|
| 1 | web/src/components/BotSettingsPanel.tsx:563 | i18n | 英文介面，bot 的 role_pending 為真：橫幅標題已翻，內文仍中文（「此 bot 啟動時未取得 daemon 位址」「設定角色」「執行中的政策」「未分配」「未知」） | 低／高 | 已開 #2216 |
| 2 | web/src/components/ApiBotCostOverview.tsx（`colorVar: '#d97706'` 等六處） | 深色 | 深色主題開成本總覽；分類色是 JS 寫死的 hex，不走 #2213 的 `--usage-*` token | 低／中（截圖上仍可辨） | 已開 #2217 |
| 3 | web/src/components/ApiTurnTimeline.tsx:2521 | i18n | `setError(err?.message \|\| '載入失敗')` 的 fallback 是中文；元件目前沒有掛載點（見 #2213 留言），實際不可達 | 極低／高 | 不另開，待掛載點決定時一併處理 |
| 4 | web/src/components/OutboxFiles.tsx、lib/htmlPreview.ts（121ba320a） | 安全（需使用者再確認） | bot 產的 `.html` 以 blob URL 在 AG Man 同源新分頁執行，可呼叫同源 API。這是使用者 2026-11-18 的明確裁示（commit 訊息已寫明「接受風險」）。若 bot 被提示注入寫出惡意 HTML，同源執行會帶來實際風險 | 中／高（風險已知、非實作錯誤） | 不開 issue；請使用者重新確認威脅模型，必要時改回沙箱或分來源 |
| 5 | 3205b0116（feat!） | 功能移除 | 網頁「最近刪除」頁與 `GET /api/bots/deleted` 一併移除，刪掉的 bot 在網頁上無法復原 | 不是缺陷；需確認有替代入口 | 不開 issue，列為確認項 |

### 不列為問題

- `schedules.css` 等檔的 `var(--warn, #b7791f)` 形式 hex 後備值：token 都已定義，後備值不會生效，無可見影響（屬衛生問題）。
- `var(--red, #e06c75)`：已在 `ebdc1e642` 前後改掉，`origin/main` 上已不存在。

### 本節小結

- 有具體觸發、可立即處理：#1（已開 #2216）、#2（已開 #2217）。
- 需要使用者裁示：#4（html 同源執行）、#5（最近刪除的替代入口）。
- 未完成：草稿／附件、排程卡片與窄螢幕的功能面（見覆蓋）。

## 5. 網頁 commit 後半（#2214 補審，`1bef36e26..a14d7e58e`，114 個）

### 覆蓋

- **精讀**：28131f39c、1e11d9d37＋ec95c9678、d8e4167fc、aec4ef485（連同 3205b0116 的脈絡）、3f352a446、c6c3e1e09＋bba9cb87b、7953bcc71、d6c524d03、43e7507f8、f3c957e63、bb3baa388。
- **抽讀**：額度列系列（773eaeb34、5215e2c72、8bbf49f04、913f27917、b873e6eb9、f5698492f、9587622c3）。
- **未讀**：其餘約 90 個 feat／refactor／test 只看標題與 diff 大小。**未讀但建議優先**：9e5d9d06a（執行者群集「全部刪除」）、3f352a446 的對話框細節、ledger／translate 複盤系列（fabdd9748、e8766990d、85a564f41、42219a58e）。

### 發現

| # | 位置 | 說明 | 嚴重度／信心 | 處理 |
|---|---|---|---|---|
| 1 | 28131f39c（去除 web 一般註解） | 同時刪掉 64 行 `i18n-ignore` 標記（約 40 檔，例 PreviewPanel.tsx 9 行、runtimeDrift.ts 9 行、QuotaStrip.tsx 7 行）。`findHardcodedCjk`（`web/src/lib/i18n/i18nScan.ts`）只給 `i18n-report` 用、不是 gate，所以影響是報表把刻意不翻的行（假資料、協定標記）全列為「寫死中文」。 | 低／高 | 不開 issue；建議把標記補回這些行。 |
| 2 | 1e11d9d37＋ec95c9678（手機前景抑制推播） | `phone_in_foreground()`（`daemon/src/webpush.rs:500`）為真時，`broadcast`（`webpush.rs:240`）丟掉**所有非 `close` 推播**，且被抑制的**不補送**；排程失敗與 ops 告警同樣受影響。手機頁面 90 秒內持續回報 visible 即抑制。若鎖屏後頁面仍回報 visible（iOS／Android 行為未驗），推播會被長期吞掉。 | 中／低（需實機） | 不開 issue；待實機：鎖屏後 `visibilitychange` 與 presence 是否停送。 |
| 3 | aec4ef485＋3205b0116（執行者單顆刪除不再確認；移除「最近刪除」頁） | 直接 `removeBot`，無確認框。後端 `guard_bot_delete`（`daemon/src/supervisor_owned.rs:59`）會擋掉被排程引用的 bot；前端也會先查排程。但最近刪除頁一併移除，網頁上**沒有還原路徑**（DB 仍是軟刪除）。 | 低；需使用者裁示（同 §4 #5） | 列入裁示。 |
| 4 | 7953bcc71（Claude 模型縮寫） | `opu-5.5`、`son-5.5`、`hai-5.5`、`fab-5.2`；只影響顯示，可讀性差。 | 低／高（主觀） | 不開 issue。 |
| 5 | 43e7507f8（換版橫幅） | 文案「請關閉後重新開啟」，按鈕是「重新載入」（`StaleBundleBanner.tsx`），文案與行為不一致。 | nit／高 | 不開 issue。 |
| 6 | 額度列系列（773eaeb34、5215e2c72、b873e6eb9、f5698492f、9587622c3 等，至少 7 個 commit 反覆改收合與量測） | 最後狀態有 jsdom 測試，但 jsdom 不做真版面；桌機窄寬與手機 390px 沒有截圖證據。 | 低／中（無具體觸發） | 不開 issue；建議截圖確認。 |
| 7 | f3c957e63（agy 基準複本清除） | `remove_script`（`crates/am-lifecycle/src/lifecycle/agy_session.rs:281`）以 `rm -rf` 刪路徑，路徑由 DB 的複本 id 組成。已有「不跟隨 link、只刪指定複本」的測試。 | 低／中 | 建議確認 id 只可能是 ULID。 |
| 8 | bb3baa388（英文判定 CJK ≤ 1/4） | 中文技術回覆若拉丁字母 ≥ 4 倍 CJK 字，會被判英文而預先翻譯（中譯中）。抽樣三句中文技術句比值 1.5–2.9，未觸發。 | 低／低 | 不開 issue。 |

### 已確認無問題

- d6c524d03（預設帳號改名 ag0）：舊資料 `agy` 名稱照解析，有測試。
- d8e4167fc（新模型推播）：第一次只記 seen 不推；抓取失敗不動 seen；開關關閉仍更新 seen。
- 3f352a446（從本機複製身份）：只複製設定與 alias，不碰 token 與登入檔（`docs/UI-DECISIONS.md` 明載）。
- c6c3e1e09＋bba9cb87b（一鍵升級＋重啟）：確認框列出要安裝、要重啟、忙碌中的排程；全部安裝成功才重啟；忙的 bot 排程閒下來重啟。

## 6. api_bot 其餘 fix（取代 §3；`grep -E '^[0-9a-f]+ fix'` 扣掉已讀）

`git log --since=2026-11-11 --grep 'api.bot\|openai-api'` 中 `fix` 前綴共 30 個，扣掉 1801dad13、32036abb1、2c1ac676b 之後 **30 個都看過**（其中 4 個只讀到關鍵段落，已註明）。

| sha | 結論 |
|---|---|
| 598caf7da 圖片／PDF 不再 400 | ✅ 只擋 bot 請教帶附件；使用者附件交 native 驗格式與大小。 |
| 797a8f2fd 撞上限正常完成 | ⚠ 結果可能被判「任務成功」→ **#2218** |
| 41172e5f0 每小時上限不限使用者回合 | ⚠ 排程回合也標成 user 類、繞過上限 → **#2219** |
| 76466db1e 明確拒絕收掉 api_pending | ✅ `definitive_rejection`：4xx（408 除外）、429、529 才算確定沒處理；逾時、斷線仍 unknown。 |
| b0eb6a29a owner failed 後結算 api_pending | ✅ 規則集中於 `pending_settled_after_failed_owner`；tool_pending 永不放寬。注意：結算後下一回合重送同一批訊息即重新計費（attempt 保留 unknown 預留，偏保守）。 |
| 8f93e251a 壓縮後修剪＋增量上界 | ✅ 上界取 `min(整包 bytes+8192, 前次 usage＋增量)`，前次 usage 含 cache 讀寫，壓縮回應排除。 |
| 1439f83b9 壓縮 iterations 驗證與計價 | ✅ 有 iterations 時逐項讀取，缺欄位 `UntrustedUsage`。 |
| 2bcc1aa07 thinking 綁定丟棄 | ⚠ **需裁示**：所有請求一律帶 `thinking: adaptive` 與 `block_binding`，先前只有 Fable 帶（測試原本斷言 Opus 不帶 thinking）。Opus 5.5 因此開始思考，output 計費與延遲上升，commit 訊息未提。另加 beta header `thinking-binding-controls-2026-08-01`，外部行為本環境未驗證。 |
| cfdf2f061 壓縮 iteration 計價 | ✅ cache 讀寫依欄位計價，有測試。 |
| f6243f1b2 凍結 system prompt | ⚠ 未逐段讀（753 行）。看到 checkpoint 有效性判斷新增 `system` 欄位。 |
| b48e818ce 保留 4xx/5xx 細節、前次用量估預留 | ⚠ 只讀到 envelope 帶 `previous_response` 與 `failure_explanation` 改動（481 行，未逐段）。 |
| de45f30ec claude-api fast 設定可存 | ✅ 寫入 config 與 DB，有測試。 |
| ae764ef02 母子模式頂層對頂層請教 | ✅ 走 `api_bot_consultable` 政策；非 bot dispatch 維持舊判斷。 |
| a7ba29812 openai-api 網頁與傳輸層收尾 | ✅ `cache_ttl` 對 openai-api 回 `cache_ttl_unsupported_for_kind`，不再誤報 api_bot_only。 |
| ff8c39dbd 測試 import 修正 | ✅ 只動測試模組。 |
| 7b700a406 送 prompt 不重複 start | ✅ 回 `api_run_already_active`，與 32036abb1 同一路徑。 |
| 153bf28ff 模型清單單一來源 | ✅ `openai_api_efforts_for_model` 單一來源；provider 建不起來回具名錯誤。 |
| 4f86728c0 設定頁單回合上限 | ✅ 0 拒絕、null＝不設（同 1801dad13 題）。 |
| 6240aa16a openai 串流進度與 Claude 一致 | ✅ 有 paced server 測試。 |
| 4dc72dafb completed 缺 usage 等 | ✅ 缺 usage 回 `MissingUsage`；預留上界用 wire bytes＋8192（bytes ≥ tokens，偏保守）。 |
| fef09ef42 終態後雜訊、Luna 長文價 | ✅ 終態後事件不解析；長文價門檻 272k、12h 快取寫入價。計價數字未對官方價表核對。 |
| 682bdbde7 串流中途 error → StreamAborted | ✅ 不當成確定拒絕，計費保守。 |
| 3266526c5 強制中斷結清整批 tool_result | ✅ 有測試。 |
| a85c5bb38 owner 無 receipt 不隔離全部 bot | ✅ 有 `owner_settled` 判斷與測試。 |
| be3ad399f 壓縮後重啟不隔離全部 bot | ✅ 壓縮前的工具 id 不再算未結清（`declared_in_responses`）。 |
| 62ab6d042 sweeper 不動進行中回合 | ⚠ 看了 `has_live_turn` 與 stale reserved 只算帳務；generation 圍欄細節未逐段讀。 |
| 74ecebdb4、49e013091 換模型後可接續 | ✅ 換模型先 `prove_on`，不支援才 409 `api_model_changed`。依賴 drop_block 處理舊 thinking 綁定（見 2bcc1aa07）。 |
| 9425ca408 回應內容不合法仍記 recorded | ✅ `invalid_content_reason` 檢查 model、tool 數（>32）、重複 tool id。 |

## 7. d143c2bc3 對 fixture（#2123 相關，`tui_prompts.rs`）

**fixture 現況**：實機 2.1.326 fixture 只有 `claude-2.1.326-failed-empty-hint.ansi`、`claude-2.1.326-failed-typed.ansi`、`claude-2.1.326-trust.ansi`（9c3165313）。**沒有** queued 與 plan 卡片的實機擷取。

| d143c2bc3 內容 | 現況（origin/main） | 判定 |
|---|---|---|
| `norm_line` 去掉 `✘⚠✔` 前綴 | 保留 | ✅ 沒有 fixture 反證。 |
| `composer_is_idle` 的 `❯`＋placeholder 分支、`awaits_input` 同樣分支 | **已撤回**（7d4c38a2d）。原因：含 try／retry／`/help` 的使用者草稿被當成空框。`is_claude_placeholder` 已無引用。 | ✅ 回歸已修。 |
| 空框失敗提示判讀 | 以實機 fixture 重做（9c3165313）：dim 字看樣式，plain 只認完全相等。 | ✅ |
| `is_status_chrome` 加 `Queued`／`queued` | 保留，無 2.1.326 實機 queued 畫面（`claude-2.1.287-bash-with-held-queued.txt` 是舊版）。`starts_with("queued")` 也會把使用者草稿開頭為 queued 的行當狀態列忽略。 | ⚠ 未驗證，低。 |
| `has_prompt_cue` 加 `trust this folder`、`trust the contents`、`use arrow keys` | 保留；`screen_has_answerable_prompt` 先看 `composer_is_idle`，空框時不看提示字，風險低。 | ✅ |
| `is_plan_approval_dialog`（+ `permission_prompt` 回 `"Plan"`） | 文字來自 changelog（"Yes, and switch to default (ask each time) for this session"／"No"）。測試畫面是合成的，且選項**無編號**。若實機有 `1.` 編號，`line_starts_with` 失敗，卡片不被辨識。 | ⚠ 需實機 plan 卡片 fixture。 |
| 測試 `TRUST_PROMPT`（合成） | 文字是 "Do you trust the contents of this project?"＋"Settings files that will be loaded:"，與實機 2.1.326 信任畫面（"Quick safety check…"，`claude-2.1.326-trust.ansi`）不同。測試仍過，因為其他 cue 命中。 | ⚠ 低：合成測試不忠實；真實畫面由 9c3165313 釘住。 |
| 符號前綴的 switch-model 測試（合成） | 依 norm_line 規則通過 | 低。 |

## 結論（完整版，2026-11-26；取代前文 §3、§4 原結論）

**已開 issue**（本輪新增）

- #2216 Bot 設定「待套用」橫幅英文仍中文（前半 §4 #1）
- #2217 成本總覽分類色寫死 hex，深色不跟 token（前半 §4 #2）
- #2218 撞 API 呼叫上限的排程可能被判「任務成功」（後半 §6，797a8f2fd）
- #2219 排程回合繞過每小時請求數上限（後半 §6，41172e5f0）

**需使用者裁示**

- html 同源執行（前半 §4 #4）：121ba320a 已寫明接受風險，請重新確認威脅模型。
- 「最近刪除」頁移除後的還原入口（前半 §4 #5、後半 §5 #3）。
- Opus 5.5 是否應開 adaptive thinking（§6 2bcc1aa07）：先前不帶，現在每回合帶，計費與延遲會變。
- claude-api 與 openai-api 的總上限、每小時上限預設都不設（§6 41172e5f0、1801dad13）：若接受，請在文件標明。

**待實機**

- 手機前景推播抑制（§5 #2）：鎖屏後是否仍抑制、是否吞掉排程失敗。
- plan 卡片與 queued 的 2.1.326 實機 fixture（§7）。
- 額度列系列改版（§5 #6）：桌機窄寬與手機 390px 截圖。

**候選（低，不開 issue）**

- 93ac0fb18 SQL 判準寬；68eb5cc55 游標不可見時仍送 Down；has_prompt_cue 的 PageUp／PageDown 寬鬆規則（前半）。
- 28131f39c 遺失 64 行 `i18n-ignore`（§5 #1）；7953bcc71 縮寫（#4）；43e7507f8 文案（#5）；bb3baa388 門檻（#8）；f3c957e63 rm 路徑（#7）。

**未覆蓋（本報告仍不完整，#2214 不應視為完成）**

- 網頁前半：草稿與附件、排程卡片、窄螢幕逐段 diff（前半 §4 已列）。
- 網頁後半：約 90 個 feat／refactor／test 只看標題；優先未讀 9e5d9d06a（執行者「全部刪除」）與 ledger／translate 複盤系列。
- api_bot：f6243f1b2、b48e818ce、62ab6d042 未逐段讀（§6 已標）。
- 前文 §1–§3 的「未讀」標記已由 §6 取代，§3 標題保留原樣未改。

### 補審：草稿與附件、排程卡片、窄螢幕（逐段讀 diff，#2214）

唯讀。讀過的 commit：草稿 `568864c5e`、`0f95cb08b`、`53a5b6783`、`93fa7af52`、`4f3575ac6`；排程 `0cf5ae7b2`（何時執行的時段列與 CSS）、`2232c454a`（固定三行卡片）、`ada29fd47`、`2d9550bcb`；附件草稿 `0b3b4fb8b`。

| # | 位置 | 類別 | 觸發情境 | 嚴重度／信心 | 處理 |
|---|---|---|---|---|---|
| 6 | web/src/schedules/format.ts `scheduleRowLines`、`rowDot`（2232c454a） | 功能 | 排程正在執行時在列上停用：卡片變「已停用 · 原訂…」、狀態點灰，看不到執行中與已進行時間 | 中／高 | 已開 #2220 |
| 7 | web/src/store/store.ts「編輯後送出」（93fa7af52，`sendPrompt` 的 clear 分支） | 確認項 | 按了「編輯後送出」，框裡的字整段換掉後送出：終端那段草稿會先被 `clear_draft` 清掉，原本的草稿不會再被送出，也沒有額外確認。設計上是「改過的字取代終端草稿」，但使用者可能只想清框 | 低／中 | 不開 issue；請確認這是預期 |
| — | web/src/schedules/schedules.css（0cf5ae7b2、2232c454a） | 窄螢幕（未驗證） | 側欄卡片的 `.project-schedule-head.with-side` 保留右側 12em 給絕對定位的 bot 晶片與 ⋯ 選單；窄側欄時名稱會被擠得很短 | 未定 | 未開。mock 沒有排程資料，無法在瀏覽器截到，列為待驗證 |

**草稿（已讀，未發現可見回歸）**：`composerDrafts` 的 `editing`／`editNow` 旗標在 `loadComposerDraft` 的合併（`cur.token === block.token` 才保留）與 `probeComposerDraftOnce`（有 editing 就不探測）裡都有保護；`draft_changed` 回來的新草稿會取代舊列（不保留 editing，但之後的送出會被 daemon 的 409 擋住，不會把終端與網頁兩段字接在一起）。`0b3b4fb8b` 的附件草稿以 `key` 去重、以世代號處理晚回來的結果，未見明確問題。

**排程時段列（已讀）**：0cf5ae7b2 的新文案都有 en 對照（逐一查過）；舊的 `.schedule-link` 樣式已無引用，不是遺漏。「何時執行」的時區與隨機區間驗證沿用原本的 `rangeError`。

### 本節小結（更新）

- 具體觸發、已開 issue：#2216（英文橫幅中文）、#2217（分類色，已修）、#2220（停用時執行中狀態被蓋掉）。
- 需要使用者確認：#4（html 同源）、#5（最近刪除的替代入口）、#7（編輯後送出會清掉終端草稿）。
- 待驗證：#—（窄側欄排程名稱）需要有排程資料的 mock 才能在瀏覽器確認。
- 仍未完成：窄螢幕的其他版面（聊天、設定面板）沒有逐段讀；草稿與附件的其餘 commit（`0f95cb08b` 的完整行為）只看了 diff，沒有在瀏覽器實際操作。
