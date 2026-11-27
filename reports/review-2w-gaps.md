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

## 7. 網頁後半最後一批（#2214 收尾，2026-11-26，唯讀）

### 覆蓋

- **精讀**：`9e5d9d06a`（群集「全部刪除」，連到 store `removeBot`、`api.deleteBot`、daemon `delete_bot` 與 `projection::delete_from_config`）；複盤系列 `fabdd9748`、`e8766990d`、`85a564f41`、`42219a58e`（權限矩陣、寫入路徑、快取與清單）；`07753571b`（host baseline 的寫入與安裝路徑、auto gate）；`9c718e3af`＋`3dc3d1ef3`（agy 自動壓縮）；`fb80eb9e8`（Claude 到期偵測與推播路徑）；`d380c3569`（agy GUI 登入自動選單的守門）。
- **抽讀（關鍵行）**：`663260b48`（openai-api 預設模型）、`7e559a4a4`（ops-alerts 收窄）、`20a0674a2`、`86949766b`、`687dd9522`、`f2c6640f5`、`88e23f53b`。
- **只讀 web 標籤**：`459e72090`（ledger 待部署／已部署）。daemon 的 `ledger_deploy.rs`（156 行）**未逐段讀**。
- **只看標題**：其餘 85 個 web commit（`1bef36e26..a14d7e58e`，扣掉前面各節已提及者），列於本節附錄。這 85 個**未審**。

### 發現

| # | 位置 | 類別 | 觸發情境 | 嚴重度／信心 | 處理 |
|---|---|---|---|---|---|
| 1 | 9e5d9d06a（`ExpertClusterRow.tsx` `deleteAll`＋`store.ts` `removeBot`） | 功能（會靜默少刪） | 群集成員在 AGM 專案裡（`supervisor_owned::load` 把整個 AGM 專案的 bot 都算 AGM 的）。每顆都回 409 `supervisor_owned`，`agmDeleteAsk` 是單一欄位、後到的覆蓋先到的，對話框只剩一顆；這條路徑不 `notify`，其餘幾顆既沒刪也沒提示；確認框卻寫「全部 N 顆」。 | 中／高 | **已開 #2221** |
| 2 | 1e11d9d37＋ec95c9678＋fb80eb9e8（登入到期提醒） | 功能（一次性通知永久漏送） | `apply_host_expiry` 先 `admit_login_expiry` 把 stage 記成已送，之後 `broadcast` 因手機前景被丟，stage 不會再送。手機在前景時剛好跨過 3 天／1 天門檻，該次提醒就沒了（`expired` 會再送一次）。 | 低～中／高 | **已開 #2222**（延伸 §5 #2） |
| 3 | 07753571b（host baseline auto） | 安全（需使用者裁示） | `[host_baseline.sync]` 設 `auto` 的主機，缺工具會**不經逐次確認**執行 `curl … \| sh`／`… \| bash` 類安裝（bun、rtk；rtk 指向 `master` 未釘版）。sudo 項只列手動，缺檔寫入只在檔案不存在時寫且有備份，這兩點看過沒問題。預設是 off，要使用者主動開。 | 中／高（風險是設計選擇） | 不開 issue；請使用者裁示是否接受、rtk 是否釘版 |
| 4 | 9c718e3af＋3dc3d1ef3（agy 自動壓縮） | 功能（自動送出指令） | agy 的 bot **不限 primary**（`plan` 條件加了 `kind == "agy"`），預設 400k；已用量超過就在 idle 且輸入框空時送 `/compact` 進 agy 的 pane，不寫續行註記。**未驗**：agy 是否真的支援 `/compact`；`context_window.total_input_tokens` 是目前上下文還是累計值（若累計，會反覆觸發）。 | 中／低（需實機） | 不開 issue；請裁示預設是否開、並安排實機確認 |
| 5 | 28131f39c（去除 `i18n-ignore`） | 工具一致性 | 見下節「28131f39c 的 i18n-ignore 結論」。 | 低／高 | 不補回；需決定機制（見結論） |
| 6 | 663260b48（openai-api 預設模型改 `gpt-6.2-astra`） | 確認項 | 新建 openai-api bot 的預設模型與 effort 改了。既有 bot 是否被改寫未驗。 | 低／中 | 不開 issue；請確認這是預期 |
| 7 | 9e5d9d06a 附帶 | nit | 每顆刪除都各發一則「已刪除 X ［復原］」通知，N 顆就 N 則，沒有批次摘要。`selectedBotId` 以批次前的快照算 `next`，並行回應順序不定，可能留下指向已刪 bot 的選取（UI 後果未在瀏覽器驗證）。 | 低／中 | 不開 issue |
| 8 | 42219a58e、e8766990d（複盤存原文／譯文前 1500 字） | 隱私（已是既有快取） | 複盤寫在譯文快取檔（0600，同一個檔）。新增的是明文文字副本，但檔案本身原本就存譯文；沒有新的外流路徑。 | 無 | 不列為問題 |

### 已確認無問題

- **權限**：`fabdd9748`、`e8766990d` 的 `GET /api/translate-records`、`/api/translate-retros`、`/api/messages/{id}/translate-record` 在 `BOT_ROUTE_POLICIES` 與 `bot_scope_matrix` 都是 UserOnly／Forbidden（與 API.md 同步）。
- **寫入**：複盤存檔在翻譯 single-flight 鎖內讀改寫；`save` 重寫時沿用既有 `retros`（有測試）。快取 key 是 `sha256(id + 內容)`，不同訊息不會互相覆寫。
- **baseline 寫入**：`WRITE_FILE` 只在目標不存在時寫（`[ -e ] → present`），有 `ba_backup`，目標是符號連結就跳過。
- **GUI 登入（`d380c3569`）**：只在畫面**精確**是「Select login method」選單時才送鍵；授權網址只收 `https://accounts.google.com/o/oauth2/…`（有反例測試）。
- **ops-alerts 收窄（`7e559a4a4`）**：一般 bot 403 `role_required`；`scripts/ops/*` 走 service principal。與 API.md／SPEC.md 一致。
- **分享 bot 預設 400k（`20a0674a2`）**：只補 `context_tokens` 的門檻；`runtime_overlay` 用的是不含 kind 預設的 `effective`，所以不會多出啟動參數。
- **agy 自動壓縮的 anchor（`3dc3d1ef3`）**：改為上一個完成回合（排除保溫 crid），同一回合不重複壓縮。
- **ledger（`459e72090`）**：只改標籤與資料欄位，web 有 DOM 測試。

### 28131f39c 的 i18n-ignore 結論：**不補回**

- `AGENTS.md` 第 13 行：禁止在程式碼寫註解，**功能性指示除外**，列舉的清單裡沒有 `i18n-ignore`。`28131f39c` 照這條規則刪掉 64 行，是符合政策的。
- 補回會直接違反 `AGENTS.md`，所以我沒有推 main。
- 但工具端還在依賴它：`web/src/lib/i18n/i18nScan.ts:94` 與 `docs/FRONTEND.md:131,136,170` 都寫明用 `i18n-ignore` 排除，`i18n-report` 現在會把那 64 行（假資料、協定字串、regex）全列為「寫死中文」。`i18n-report` 不是 gate，影響只在報表。
- **需要決定**（三擇一）：(a) 把 `i18n-ignore` 加進 `AGENTS.md` 的功能性指示清單，再把 64 行補回（小改，可立即做）；(b) 把排除清單移到資料檔（例如 `web/scripts/i18n-allow.txt`），程式碼保持無註解；(c) 接受報表雜訊，改掉 `FRONTEND.md` 那幾段。我傾向 (b)，這樣程式碼與政策都不用改。

### 本節小結

- 開 issue：#2221（群集全部刪除靜默少刪）、#2222（登入到期提醒永久漏送）。
- 需使用者裁示：#3（auto baseline 的 `curl | sh`，rtk 是否釘版）、#4（agy 預設自動 `/compact`，待實機）、28131f39c 的 i18n-ignore 機制（三擇一）、#6（openai-api 預設模型）。
- 未完成：附錄 85 個 commit 只看標題；`ledger_deploy.rs` 未逐段讀。

## 附錄：未審的 web commit（只看標題，2026-11-26）

- `2fdc129ea` feat(web): API 餘額在額度列與選單上全域可見�
- `81594406f` fix(web): Claude 餘額晶片標籤改 Claude、金額不�
- `663260b48` feat(api-bot): openai-api 模型擴充為 gpt-6.2-astra（
- `66892d3b6` fix(web): 手機對話區水平超版（openai token 拆�
- `f14e9e33c` feat(web): 餘額列改用圖示、openai-api 側欄同 co
- `c2267fbc6` feat(reset-credit): 重置券提示可「不再提醒」1 
- `42a6e355e` feat(web): claude-api 與 openai-api 圖示改用實心 AP
- `54102b6d5` feat(web): claude-api 與 openai-api 圖示改用右側實
- `bfe8b4883` refactor(web): share inert file methods in share page test
- `7b4bb6560` feat(web): 統一 codex 與 openai-api 模型命名並依�
- `8df02e2ca` refactor(sidebar): 合併側欄 bot 列渲染與重複的 
- `24e30af31` refactor(web): 收斂選單與彈出框點擊外圍/按Esc
- `a1dc7d069` feat(identities): 新增帳號對話框（建議目錄、�
- `5e7498690` refactor(web): 身份頁版面骨架：主機分頁＋CLI 
- `deeb02ed6` feat(identities): 動作命名、「⋯」選單與確認�
- `f68c5f16a` feat(web): agy 模型清單依價格由便宜到貴排序�
- `a737c2c7c` feat(identities): 新增帳號對話框接上身份頁（�
- `79f071933` fix(identities): 收尾文案與手機排版
- `c9c2b8c7f` fix(web): agy 目前模型不在清單時補全顯示並�
- `4f9886d89` fix(web): 模型清單備用提示涵蓋 other-host 與 st
- `9724d2d26` feat(web): 訊息翻譯切換改為微移淡入與圖示�
- `5bbb8b01f` feat(agy): prompt cache 倒數（12 分 TTL），不進保
- `c83e90007` fix(agy): 進行中泡泡顯示 agy 活動列（讀檔／�
- `b1047a48f` feat(translate): 訊息翻譯「重新翻譯」按鈕＋�
- `d3880bd24` fix(agy): Gemini 4 Argon 週額度不再蓋掉 Gemini 週�
- `d27177e3d` feat(web): agy Argon 改為血條列與 Ar 標籤
- `9c7596ed7` refactor(web): 刪除死碼 dateTimeLabel 與 dropBefore�
- `2b8c0646e` refactor(web): 測試 harness 共用化：未讀規則、m
- `c00748a10` refactor(web): 抽 CapEditor 共用總花費上限編輯�
- `81d37e282` fix(agy): Orchestrator slug（gemini-pro-orchestrator-*）
- `e8eb454bf` feat(web): 手機額度列改緊湊卡片（方案 1，390
- `00b09397b` feat(web): 手機 agy Argon（Ar）比照 Fable：low／cr
- `0cc630f95` chore(web): 額度彈窗移除 agy 登出鈕（使用者�
- `fc76cd8fc` fix(web): agy popover 只有 Argon 有讀數時不寫「�
- `3f794e477` chore(web): 移除 agy 登出鈕後的孤兒 i18n 字串
- `e75cfd00c` feat(web): agy 模型選單拆成「模型＋強度」兩�
- `7fddc5940` test(web): blobUrlCache／outboxPreviewCache 測試不再�
- `c450a0103` fix(web): 額度列餘額晶片數字一律 toFixed(1)
- `d8cadea59` feat(translate): 譯文「複盤」——完成的譯文�
- `c2ee11a18` feat(web): 使用者訊息旁逐項帳本進度列，支�
- `885aefdd2` fix(web): 手機底部輸入列在 safe-area 之外不再�
- `bf8b42cbd` fix(web): 單筆未分流的帳本進度列改顯示「加
- `814476e0e` fix(web): 手機換版橫幅與通知改到螢幕垂直中
- `6a6af606f` fix(web): 手機輸入列左右內距避開 iPhone 螢幕�
- `94fecdaab` feat(web): 帳本追蹤任務 UI 重做（白話摘要＋�
- `c3e49701c` feat(web): 對話窗上方的「追蹤中」列（本專�
- `c6dba5659` fix(ledger): 預設不自動收件；使用者訊息可手
- `1eeca5e9e` feat(ledger): 待驗收（answered）：管理者交件、
- `3bb0a67b0` Revert "fix(web): 手機輸入列左右內距避開 iPhone
- `773c0f3cf` feat(web): 專案血統清單與快取節省顯示及統�
- `fb69cb281` fix(ledger): 待驗收的「還沒好」改為「不知道
- `ae511749a` fix(agy): 背景工作數改讀底部狀態列，側欄能
- `f7b11ebc3` fix(web): 手機追蹤任務面板改掛到 body，不再�
- `fb80eb9e8` feat(auth): detect Claude OAuth refresh token expiry and p
- `df663a090` fix(web): center Claude expiry notice on mobile and locali
- `771b5a03b` fix(web): 自動翻譯只有選取文字時保留原文，
- `a923fa66c` fix(web): 加入追蹤選單改用不透明面板底色與
- `e4297bd1c` feat(web): 事項代碼改為左側固定寬度膠囊並�
- `1c8f97379` fix(web): 側欄專案卡改為顯示被停用的身份名
- `d7af5803a` feat(hosts): 移除 Grok baseline 項目，補 darwin 去�
- `9c718e3af` feat(daemon,web): support context_tokens and automatic com
- `9dd623538` fix(hosts): #719 auto 投影到 web、移除舊 accessor�
- `88e23f53b` fix(agy): 身份登入後立即重新探額度、未登入
- `0e6296e91` fix(identities): 遠端同步納管 coN/gkN/agN alias，�
- `3dc3d1ef3` fix(agy): agy 沒設 context 時預設 400k（自動壓縮
- `d380c3569` feat(agy): 網頁 GUI 登入輔助——登入 pane 自動
- `459e72090` feat(ledger): 待驗收分待部署與已部署（deploy_s
- `5d738b155` feat(web): 側欄執行者 Wkr 精簡（單行群集、活
- `77cc819fd` fix(web): 英文回覆的表格列不再壓低「主要是
- `86949766b` feat(cache): 將 agy prompt cache TTL 改為 1 小時（36
- `d8c99894d` feat(web): 手機版通知改為輸入列上方輕量膠�
- `20a0674a2` feat(daemon,web): 分享功能 bot 於 bot menu 標示且�
- `afee7d3be` feat(web): 側欄 Worker 區執行者群集改卡片＋kin
- `d132e8202` feat(web): 專家模式把分享 bot 獨立成「分享用
- `11a009778` fix(web): 終端機連結不再跳原生 confirm，並禁�
- `07753571b` feat(baseline): 缺檔照模板寫入、缺工具用官方
- `687dd9522` fix(web): 保留 reset_offer_snoozed_until，修正重置�
- `f2c6640f5` feat(schedules): 排程基準快照支援 agy，無基準�
- `c56079f55` fix(web): 右下角升級通知精簡成一行，細節留
- `60eb85094` fix(web): 升級通知文字改由前端 i18n 組字，英
- `bad3b6e17` feat(web): 排程最近執行顯示每次執行當時的 b
- `e493ad282` fix(web): 排程編輯器窄螢幕不再橫向溢出，最
- `925d803c4` fix(web): 排程編輯器深色主題改用主題 token，
- `7d0e3c35b` fix(update): 一條龍升級同一台的 claude 與 codex 
- `7e559a4a4` fix: fb-scan 第 4、6、8 條（血統區塊主題 token�

## 結論（最終版，#2214 收尾，2026-11-26）

**這份報告取代前面所有「結論」段**（前文各段保留原樣，不改寫）。

### 已開 issue（#2214 全部批次）

| issue | 內容 | 狀態 |
|---|---|---|
| #2216 | 待套用橫幅內文英文仍中文 | 已修（742ae9568）並關閉 |
| #2217 | 成本總覽分類色寫死 hex | 已關閉 |
| #2218 | 排程回合撞 API 上限可能被判「任務成功」（797a8f2fd） | 開啟 |
| #2219 | 排程回合繞過每小時請求數上限（41172e5f0） | 開啟 |
| #2220 | 排程停用時，執行中狀態被蓋掉 | 開啟 |
| #2221 | 群集「全部刪除」在 AGM 專案內只刪到一顆（9e5d9d06a） | 開啟 |
| #2222 | 手機前景抑制推播，一次性的登入到期提醒永久漏送（fb80eb9e8 × ec95c9678） | 開啟 |

### 需使用者裁示

- **html 同源執行**（121ba320a）：使用者 2026-11-18 已接受風險，請再確認威脅模型，必要時改回沙箱或分來源。
- **「最近刪除」頁移除後的還原入口**（3205b0116、aec4ef485）：目前網頁上沒有還原路徑。
- **Opus 5.5 每回合帶 adaptive thinking**（2bcc1aa07）：計費與延遲會變，commit 訊息未提。
- **claude-api／openai-api 每回合與每小時上限預設不設**（1801dad13、41172e5f0）：總上限已確認存在（`cap.rs total_cap`）；每小時上限與預設不設的部分，請標在文件。
- **host baseline 的 auto 模式**（07753571b）：會不經逐次確認執行 `curl | sh` 類安裝；rtk 指向未釘版的 `master`。預設 off。
- **agy 預設自動 `/compact`**（9c718e3af、3dc3d1ef3）：所有 agy bot 預設 400k，超過就送 `/compact`；未實機驗證。
- **28131f39c 的 i18n-ignore**：不補回（違反 `AGENTS.md` 的註解政策），需從 (a)(b)(c) 三擇一決定工具機制，見 §7。
- **openai-api 預設模型改 gpt-6.2-astra**（663260b48）：請確認是預期。

### 待實機

- 手機前景推播抑制（§5 #2、#2222）：鎖屏後是否仍抑制、是否吞掉一次性通知。
- plan 卡片與 queued 的 2.1.326 實機 fixture（§7，前半）。
- agy 的 `/compact` 是否可用，以及 `total_input_tokens` 的語意（§7 #4）。
- 額度列改版、窄側欄排程名稱：桌機窄寬與手機 390px 截圖。

### 候選（低，不開 issue）

- 93ac0fb18 SQL 判準寬；68eb5cc55 游標不可見時仍送 Down；`has_prompt_cue` 的 PageUp／PageDown 寬鬆規則（前半）。
- 9e5d9d06a 的通知逐顆一則、選取可能指向已刪 bot（§7 #7）。
- 28131f39c 遺失 64 行 `i18n-ignore`（§7，需決定機制）；7953bcc71 縮寫；43e7507f8 文案；bb3baa388 門檻；f3c957e63 rm 路徑。

### 未完成（#2214 關閉時的誠實範圍）

- **網頁後半 85 個 commit 只看標題**（附錄，§7）。優先的 `9e5d9d06a` 與複盤系列已精讀；其餘依風險抽讀，多數沒有看 diff。
- `ledger_deploy.rs`（459e72090，156 行）未逐段讀。
- api_bot：`f6243f1b2`、`b48e818ce`、`62ab6d042` 未逐段讀（§6 已標）。
- 網頁前半的草稿與附件、窄螢幕各版面只做了 diff 層級的審查（§4、§5 補審）。

這些未完成項目都列在上面，沒有遺漏；若需要另一批審查，請從附錄開始。

## §8 補審（2026-11-27）

唯讀。對象是上面「未完成」列出的四項。行號以 origin/main `3efab16ac` 為準。已在 #2216–#2224 處理的不重報。
開出的 issue：**#2230**（發現 1）、**#2231**（發現 3）。其餘發現嚴重度或信心不到門檻，只列在這裡。

### 覆蓋

| 範圍 | 覆蓋 | 說明 |
|---|---|---|
| `f6243f1b2` api_bot system 固定 | ✅逐段 | 正式程式的 diff 全讀，加上現行 `runtime.rs:660-700`、`:2675-2703`、`prompt.rs:50-65`。測試只看名稱。 |
| `b48e818ce` 預留與 HTTP 錯誤 | ✅逐段 | `http.rs`、`completion.rs`、`quote.rs`、`pricing.rs`、`adapter.rs`、`runtime.rs` 的 diff 全讀，加上現行 `pricing.rs:100-232`。 |
| `62ab6d042` sweeper 圍欄 | ✅逐段 | `sweeper.rs`、`runtime.rs` 的 diff 全讀。沒有發現問題。 |
| `459e72090` `ledger_deploy.rs` | ✅逐段 | 現行檔案 156 行全讀，加上 `ledger_api.rs` 的呼叫端、`deliverable_shas`、`normalize_deploy_sha`、`deploy_now::git`。 |
| 附錄 85 個 web commit | 14 個讀過（其中 3 個是 fb-scan 已讀）、33 個 ✗未讀、38 個只看標題 | 明細見下方三張清單。 |
| 草稿與附件 | ⚠抽樣 | 讀了現行 `composerAttachmentDrafts.ts` 全檔與 `attachmentsHelpers.ts:100-190` 的還原路徑，對過伺服器端附件保留時間（`attach.rs:356`，24 小時）。`0f95cb08b` 補打 composer-draft 的路徑沒有再讀。 |
| 窄螢幕版面 | ✗未讀 | 沒有在瀏覽器實際操作，也沒有逐段讀 CSS。 |

### 發現

| # | sha、檔:行 | 觸發情境 | 嚴重度／信心 | 建議修法 | issue |
|---|---|---|---|---|---|
| 1 | `5bf73eeca`（疊在 `f6243f1b2` 上）`daemon/src/api_bot/runtime.rs:694`、`:2675-2703` | openai-api bot 在 session 中途改了指示（global／專案指示、persona、bot 名稱）。更新區塊與 prompt 接成同一個字串，`already_updated_with` 用完全相等比對，永遠不成立，之後**每個回合**都再附一次完整指示，直到 session 重置。claude-api 不受影響。 | 中／高 | 字串內容改用「等於或以區塊加分隔開頭」比對，或 openai 也送成獨立 part。補 openai 三回合測試。 | #2230 |
| 2 | `f6243f1b2` `daemon/src/api_bot/runtime.rs:670-675` | 指示改成 Y（補了更新區塊）之後又改回原本的 X：`stored == fresh`，不再補更新，對話裡最後一份指示仍是 Y，bot 繼續照 Y 做，直到 session 重置。兩種 kind 都有。 | 低–中／高 | 判斷改成「目前生效的指示（最後一個更新區塊，沒有就用 stored）≠ fresh」。 | — |
| 3 | `b48e818ce` `daemon/src/api_bot/pricing.rs:132`、`:147-190`、`:217`；`adapter.rs:811` | 預留金額把上一次回覆的 prefix 用 cache 讀取價算，沒有確認 cache 還活著。bot 閒置超過 TTL（預設 5 分鐘）或換模型後的第一個請求，實際是寫入價，預留低估 20 到 30 倍；`crates/am-base/src/db/api_bot.rs:375`、`:389` 的上限檢查會放行。超支以每次冷啟動一個請求為界。 | 中／中 | 上一次回覆距今小於 TTL 且模型、effort 沒變才用讀取價，否則整段寫入價。 | #2231 |
| 4 | `b48e818ce` `daemon/src/api_bot/pricing.rs:106-127` `input_token_bound` | 增量上界只加「上一次回覆內容＋最後一則訊息」的位元組。上一次回覆之後若有超過一則訊息進了 transcript（例：前一個請求失敗但使用者訊息已寫入），上界會低估。 | 低／低（未確認這種 transcript 是否會出現） | 改成加總上一次回覆之後的所有訊息。 | — |
| 5 | `b48e818ce` `daemon/src/api_bot/completion.rs:455-580`、`http.rs` | 錯誤說明寫死「Anthropic API 回了錯誤狀態碼」，openai-api 也顯示這句；request id 只讀 `request-id`，OpenAI 用的是 `x-request-id`，所以 openai 的錯誤沒有 request id。 | 低／高 | 依 kind 換字樣；兩個 header 都讀。 | — |
| 6 | `459e72090` `daemon/src/ledger_api.rs:344-364`；`web/src/components/MessageLedgerBar.tsx:31-33` | 訊息清單附帶的 `ledger_items` 沒有 `deploy_state`（只有 `/ledger` 清單與 PATCH 回應有）。訊息下方的帳本列在載入時一律顯示「待驗收」，要等到該訊息收到 `ledger_updated` 才變成「等待部署／已部署待驗收」；訊息清單重新載入後又變回去。上方的「追蹤中」列不受影響。 | 低–中／高 | `attach_ledger_items_to_messages` 也算 `deploy_state`，或前端在掛載時補抓一次。 | — |
| 7 | `459e72090` `daemon/src/ledger_deploy.rs:54-80` | 交件內容裡的 sha 若在 push 前被 rebase 或壓掉，舊 sha 還留在本機物件庫但不是 live 的祖先，該項會一直顯示「等待部署」，直到物件被 gc。 | 低／中 | 祖先判斷為否且 sha 不在任何遠端分支上時，回 `None`（顯示一般的待驗收）。 | — |
| 8 | `459e72090` `daemon/src/ledger_deploy.rs:22-38` | `VERIFIED` 只快取成功的結果。交件內容裡不是 commit 的 7 位以上 hex 字（日期 `20261127`、編號）每次列帳本都各跑一次 `git rev-parse`；「追蹤中」列在任何專案的帳本更新時都會重抓。 | 低／高 | 失敗結果也快取（設短的有效時間）。 | — |
| 9 | `c2267fbc6` `daemon/src/reset_offer_snooze.rs`（`load`／`set`／`clear`） | 檔案是讀出、修改、寫回，沒有鎖；`load` 遇到過期項目會順手寫回。`GET /api/quota` 與推播掃描的 `load` 若與 `set` 同時發生，剛設定的「不再提醒」可能被舊內容蓋掉。 | 低／中 | 包一個行程內的 mutex。 | — |
| 10 | `c3e49701c` `web/src/components/TrackingBar.tsx:103-118` | 操作成功但緊接的重新抓清單失敗（斷線）時，顯示「帳本操作失敗」，使用者會以為沒有生效。 | 低／高 | 重新抓清單的錯誤另外處理，不顯示操作失敗。 | — |
| 11 | `a1dc7d069` `web/src/components/identityEnv.ts`（`sharedHomeWith`） | 在遠端主機新增身份時，與「共用身份」（`host` 是空的）同目錄不會出現共用登入的警告，因為只比對 `(host || 'local') === 目前主機`。 | 低／中 | 遠端主機也把 `host` 為空的身份納入比對。 | — |
| 12 | `0b3b4fb8b` `web/src/store/composerAttachmentDrafts.ts`（`openDb`、`tx`） | 每次讀寫都 `indexedDB.open` 一次，連線沒有 `close()`。長時間開著的分頁會累積連線。 | 低／中 | 連線只開一次並重用。 | — |
| 13 | `0b3b4fb8b` 同上 | 兩個分頁同時開：甲分頁送出附件後，乙分頁的記憶體仍留著同一個已上傳附件的晶片，可以再送一次。 | 低／低（未實測伺服器對重複附件 id 的反應） | 用 BroadcastChannel 通知其他分頁移除。 | — |
| 14 | `fb80eb9e8` `daemon/src/runners/claude_login_expiry.rs`（`read_local_expiry`、`read_remote_expiry`） | 只讀 `.credentials.json`。macOS 的 Claude Code 把憑證存在 Keychain，m4p 這類主機不會有到期提醒，畫面上也沒有說明。 | 低／中（未在 m4p 實測） | 讀不到檔案時在身份頁標示「無法偵測到期」。 | — |

**讀過、沒有發現問題的**：`62ab6d042`（`session_busy` 四個條件與 `revision`＋`generation`＋`updated_at` 圍欄一致）；`7d0e3c35b` 的 `update_chain.rs`（同主機排隊、失敗後不再開新的、逾時處理）；`07753571b` 的 `apply.rs`／`plan.rs`／`api.rs`（安裝指令只來自編進程式的 `baseline.toml`，plan 由伺服器產生，路由 UserOnly，目標路徑有白名單、不跟符號連結、不覆寫既有檔，sudo 項目只列手動；預設 off）；`24e30af31`（`useDismiss`，行為差異只有 `ExpertClusterRow`、`Tools` 多了 Esc 關閉）；`deeb02ed6` 現行 `IdentityRow.tsx`（確認框走 portal，有 Bot 綁定時不能移除）。

### 附錄 85 個 commit 的分類

排序依據：正式程式的 diff 行數（扣掉測試、文件、CSS、i18n），再看是否碰到狀態同步、刪除或寫入、權限、安裝。

**讀過（14 個）**

- ✅ `459e72090`、`24e30af31`、`c2ee11a18`（現行 `MessageLedgerBar.tsx` 全檔）
- ⚠ `7d0e3c35b`（`update_chain.rs` 讀完；`cli_update.rs` 與 `store.ts` 的部分沒讀）
- ⚠ `07753571b`（`apply.rs`、`plan.rs`、`api.rs` 讀完；`HostBaselineApply.tsx` 與 248 行的 statusline 模板沒讀）
- ⚠ `deeb02ed6`（現行 `IdentityRow.tsx` 前 470 行；`IdentityKindCard.tsx`、`identityRows.ts` 沒讀）
- ⚠ `c2267fbc6`（daemon 部分讀完；網頁部分沒讀）
- ⚠ `c3e49701c`（`TrackingBar.tsx` 的邏輯段；JSX 後半沒讀）
- ⚠ `fb80eb9e8`（`claude_login_expiry.rs` 主要路徑；`webpush.rs` 與網頁部分沒讀，推播部分已有 #2222）
- ⚠ `a1dc7d069`（`identityEnv.ts` 全檔；427 行的對話框只查了送出與共用目錄的段落）
- ⚠ `0e6296e91`（`identity_sync.rs` diff 的前 120 行）
- `773c0f3cf`、`f2c6640f5`、`bad3b6e17`：已在 `reports/fb-scan-2w-review.md` 讀過，這次沒有重讀。

**✗未讀，風險中以上，建議另派（33 個）**

- `5e7498690` refactor(web): 身份頁版面骨架：主機分頁＋CLI 卡＋統一列狀態（第 1 階段）
- `a737c2c7c` feat(identities): 新增帳號對話框接上身份頁（頁首／CLI 卡入口、背景登入）
- `94fecdaab` feat(web): 帳本追蹤任務 UI 重做（白話摘要＋面板，移除使用者端合併）
- `1eeca5e9e` feat(ledger): 待驗收（answered）：管理者交件、使用者驗收（不改 schema）
- `c6dba5659` fix(ledger): 預設不自動收件；使用者訊息可手動「加入追蹤」
- `fb69cb281` fix(ledger): 待驗收的「還沒好」改為「不知道」，下一輪自動複查
- `d8cadea59` feat(translate): 譯文「複盤」——完成的譯文可開複盤任務給管理者（分析翻不好的原因、改規則）
- `b1047a48f` feat(translate): 訊息翻譯「重新翻譯」按鈕＋抓部分沒翻
- `20a0674a2` feat(daemon,web): 分享功能 bot 於 bot menu 標示且預設 context 400K
- `88e23f53b` fix(agy): 身份登入後立即重新探額度、未登入不進失敗冷卻，對話框輪詢真的重驗；無讀數時顯示「查詢中…／未登入」佔位
- `8df02e2ca` refactor(sidebar): 合併側欄 bot 列渲染與重複的 BotRow props
- `c00748a10` refactor(web): 抽 CapEditor 共用總花費上限編輯表單（去重 A3）
- `5d738b155` feat(web): 側欄執行者 Wkr 精簡（單行群集、活躍優先、停派收納、整區收合）
- `e75cfd00c` feat(web): agy 模型選單拆成「模型＋強度」兩排（前端分組，存值仍是完整 slug）
- `9724d2d26` feat(web): 訊息翻譯切換改為微移淡入與圖示半旋轉動效
- `2fdc129ea` feat(web): API 餘額在額度列與選單上全域可見（m4p 等無額度格主機也看得到）
- `663260b48` feat(api-bot): openai-api 模型擴充為 gpt-6.2-astra（預設）／gpt-6.2-sol／gpt-6-luna
- `9dd623538` fix(hosts): #719 auto 投影到 web、移除舊 accessor、retry 可隨 shutdown 取消
- `9c718e3af` feat(daemon,web): support context_tokens and automatic compaction for agy bots
- `d3880bd24` fix(agy): Gemini 4 Argon 週額度不再蓋掉 Gemini 週額度（獨立 argon_weekly 欄位與晶片段）
- `3dc3d1ef3` fix(agy): agy 沒設 context 時預設 400k（自動壓縮門檻與網頁分母）；壓縮錨點改為上一個完成回合
- `c83e90007` fix(agy): 進行中泡泡顯示 agy 活動列（讀檔／執行指令），有文字時也顯示
- `ae511749a` fix(agy): 背景工作數改讀底部狀態列，側欄能看到 agy 的背景工作
- `5bbb8b01f` feat(agy): prompt cache 倒數（12 分 TTL），不進保溫／熱壓
- `86949766b` feat(cache): 將 agy prompt cache TTL 改為 1 小時（3600 秒）
- `81d37e282` fix(agy): Orchestrator slug（gemini-pro-orchestrator-*）放行，第三方仍擋；web 標籤「Orchestrator Neon/Argon」並排在 Gemini 之後
- `d7af5803a` feat(hosts): 移除 Grok baseline 項目，補 darwin 去重回歸測試（#719）
- `11a009778` fix(web): 終端機連結不再跳原生 confirm，並禁止原生對話框
- `d380c3569` feat(agy): 網頁 GUI 登入輔助——登入 pane 自動選 Google 並擷取授權網址
- `687dd9522` fix(web): 保留 reset_offer_snoozed_until，修正重置券「不再提醒」reload 與 WS 更新後重跳
- `7e559a4a4` fix: fb-scan 第 4、6、8 條（血統區塊主題 token、ops-alerts 呼叫者限制、kick 以執行中 daemon 為準）
- `60eb85094` fix(web): 升級通知文字改由前端 i18n 組字，英文不再顯示後端中文
- `c56079f55` fix(web): 右下角升級通知精簡成一行，細節留在頁首晶片

其中最該先看的：`5e7498690`（1,487 行的身份頁改寫）、`a737c2c7c`、`94fecdaab`／`1eeca5e9e`／`c6dba5659`／`fb69cb281`（帳本的待驗收與收件流程，含 daemon）、`d8cadea59`／`b1047a48f`（翻譯，含 daemon）、`20a0674a2`（分享 bot 預設）、`8df02e2ca`（側欄 bot 列合併）。

**只看標題，判定低風險（38 個）**：純 CSS、圖示、文案、排序、測試或死碼刪除，沒有讀 diff。

- `81594406f` fix(web): Claude 餘額晶片標籤改 Claude、金額不加 ≈
- `66892d3b6` fix(web): 手機對話區水平超版（openai token 拆解列 nowrap 撐寬 msg-list）
- `f14e9e33c` feat(web): 餘額列改用圖示、openai-api 側欄同 codex 綠色加虛線與 API 字
- `42a6e355e` feat(web): claude-api 與 openai-api 圖示改用實心 API 徽章加大醒目度
- `54102b6d5` feat(web): claude-api 與 openai-api 圖示改用右側實心長方形 API 徽章
- `bfe8b4883` refactor(web): share inert file methods in share page tests
- `7b4bb6560` feat(web): 統一 codex 與 openai-api 模型命名並依家族便宜到貴排序
- `f68c5f16a` feat(web): agy 模型清單依價格由便宜到貴排序（3.8 Flash < Hydrogen < Argon < Neon）
- `79f071933` fix(identities): 收尾文案與手機排版
- `c9c2b8c7f` fix(web): agy 目前模型不在清單時補全顯示並統一按鈕標籤
- `4f9886d89` fix(web): 模型清單備用提示涵蓋 other-host 與 static，支援沿用其他主機清單
- `d27177e3d` feat(web): agy Argon 改為血條列與 Ar 標籤
- `9c7596ed7` refactor(web): 刪除死碼 dateTimeLabel 與 dropBefore（連同只測它的測試）（去重 A3）
- `2b8c0646e` refactor(web): 測試 harness 共用化：未讀規則、mock bot 啟動、登入協助 DOM（去重 A3）
- `e8eb454bf` feat(web): 手機額度列改緊湊卡片（方案 1，390px 一屏全收）
- `00b09397b` feat(web): 手機 agy Argon（Ar）比照 Fable：low／critical 時取代 7d 顯示，tooltip 與面板列出 Argon
- `0cc630f95` chore(web): 額度彈窗移除 agy 登出鈕（使用者：彈窗不需要登出）
- `fc76cd8fc` fix(web): agy popover 只有 Argon 有讀數時不寫「背景查詢中」，並補測試確認 Ar 列
- `3f794e477` chore(web): 移除 agy 登出鈕後的孤兒 i18n 字串
- `7fddc5940` test(web): blobUrlCache／outboxPreviewCache 測試不再改全域 URL.revokeObjectURL
- `c450a0103` fix(web): 額度列餘額晶片數字一律 toFixed(1)
- `885aefdd2` fix(web): 手機底部輸入列在 safe-area 之外不再多墊 12px（max 取代相加）
- `bf8b42cbd` fix(web): 單筆未分流的帳本進度列改顯示「加入追蹤任務」
- `814476e0e` fix(web): 手機換版橫幅與通知改到螢幕垂直中間
- `6a6af606f` fix(web): 手機輸入列左右內距避開 iPhone 螢幕圓角（safe-area − 6px）
- `3bb0a67b0` Revert "fix(web): 手機輸入列左右內距避開 iPhone 螢幕圓角（safe-area − 6px）"
- `f7b11ebc3` fix(web): 手機追蹤任務面板改掛到 body，不再被主力列把手蓋住
- `df663a090` fix(web): center Claude expiry notice on mobile and localize text
- `771b5a03b` fix(web): 自動翻譯只有選取文字時保留原文，向上捲動不再擋住譯文
- `a923fa66c` fix(web): 加入追蹤選單改用不透明面板底色與陰影，蓋過相鄰訊息列
- `e4297bd1c` feat(web): 事項代碼改為左側固定寬度膠囊並支援雙欄對齊與深色主題
- `1c8f97379` fix(web): 側欄專案卡改為顯示被停用的身份名稱
- `77cc819fd` fix(web): 英文回覆的表格列不再壓低「主要是英文」比例，自動翻譯恢復
- `d8c99894d` feat(web): 手機版通知改為輸入列上方輕量膠囊＋精簡部署提示文案
- `afee7d3be` feat(web): 側欄 Worker 區執行者群集改卡片＋kind 色條提升辨識度
- `d132e8202` feat(web): 專家模式把分享 bot 獨立成「分享用」分段
- `e493ad282` fix(web): 排程編輯器窄螢幕不再橫向溢出，最近執行狀態字樣走 i18n
- `925d803c4` fix(web): 排程編輯器深色主題改用主題 token，不再有白底

### 仍未覆蓋

- 上面 33 個未讀的 commit，以及 8 個只抽樣的 commit 沒讀的部分。
- 窄螢幕版面（聊天、設定面板、身份頁）：沒有實機或瀏覽器驗證。
- 草稿：`0f95cb08b` 補打 composer-draft 的路徑沒有在瀏覽器實際操作。
- 發現 3、4、13、14 沒有實測；發現 3 需要一次冷啟動的實際帳單對照預留值。
- 原生 alert／confirm／prompt：對 85 個 commit 新增的 ts／tsx 行做過搜尋，沒有新增。深色主題色碼：14 個 commit 的 CSS 有寫死的 hex 或 rgb；只確認了帳本的 `messageLedgerBar.css`（有對應的深色覆寫）與 `trackingBar.css`（都有 token），其餘（`773c0f3cf` 已由 `7e559a4a4` 修、`d8c99894d`、`814476e0e`、`2fdc129ea` 等）沒有逐一確認。

## §9 補審（2026-11-27，§8 列為「未讀」的 33 個 commit＋`0f95cb08b`＋深色色碼）

唯讀。行號以 main `23d2f9696` 為準。已在 #2216–#2232 處理的不重報。
開出的 issue：**#2233**（發現 1）。其餘發現嚴重度或信心不到門檻，只列在這裡。

### 覆蓋

| 範圍 | 覆蓋 | 說明 |
|---|---|---|
| 帳本 `94fecdaab`／`1eeca5e9e`／`c6dba5659`／`fb69cb281` | ✅逐段 | 三個 diff 全讀；`94fecdaab` 的 TSX 已被後續改寫，改讀現行 `MessageLedgerBar.tsx`、`TrackingBar.tsx` 全檔，加上 `ledger_api.rs:150-369`、`ledger.rs:372-605`、`:860-946`、`ledger_reminders.rs:20-135`。CSS 只做色碼掃描。 |
| 翻譯 `b1047a48f`／`d8cadea59`／`9724d2d26` | ✅逐段（`9724d2d26` 只讀 TS 變更行） | 加上現行 `message_translate.rs:215-420`、`:540-585`、`:700-925`。`d8cadea59` 的複盤路徑已被 `fabdd9748` 取代（現行 `post_translate_report` 只收被擋的譯文，網頁改為唯讀的翻譯紀錄），不另報。 |
| 身份頁 `5e7498690`／`a737c2c7c` | ✅現行檔 | `5e7498690` 是搬檔，改讀現行 `IdentitiesPanel.tsx`、`IdentityShellRows.tsx`、`IdentityKindCard.tsx`、`IdentityHostTabs.tsx`、`identityRows.ts` 全檔與 `IdentityRow.tsx:120-554`；對過 diff 移除的函式清單，沒有遺失的功能。`a737c2c7c` 讀 `store.ts` 與 `AddIdentityDialog.tsx` 的 diff，加上現行對話框 `:84-235`。對話框 JSX 後半（`:236-482`）沒讀。 |
| `20a0674a2` 分享 bot 預設 | ✅逐段 | diff 全讀，加上 `runtime_params.rs:1-66`、`runtime_overlay.rs:40-72`、`start.rs:1556-1600`、`setup.rs:1175-1190`。 |
| `8df02e2ca`、`c00748a10`、`0f95cb08b`、`687dd9522`、`11a009778` | ✅逐段 | diff 全讀。前兩個是等價重構，沒有行為差異。 |
| `88e23f53b`、`d380c3569`、`663260b48`、`9dd623538`、`d7af5803a`、`7e559a4a4`、`9c718e3af`、`3dc3d1ef3`、`d3880bd24`、`5bbb8b01f`、`86949766b`、`c83e90007`、`ae511749a`、`81d37e282`、`60eb85094`、`c56079f55` | ⚠正式程式的變更行全讀 | 測試、fixture、i18n、CSS 沒讀；`9c718e3af`／`3dc3d1ef3`／`d3880bd24`／`d380c3569` 只看新增與刪除的行，沒有回頭讀周圍未變動的程式。 |
| `5d738b155`、`e75cfd00c`、`2fdc129ea` | ✅逐段（扣掉 CSS、測試） | diff 全讀。 |
| 深色主題色碼（14 個 commit） | ✅掃描 | 對附錄 85 個 commit 新增的 CSS 行搜尋 hex／rgb／hsl，共 165 行、14 個 commit；逐行對現行檔，扣掉 `var(--x, 備用值)` 與陰影後看有沒有深色覆寫。沒有在瀏覽器實際切主題。 |

33 個 commit 全部至少讀過正式程式的變更行。沒有任何一項是實機或瀏覽器驗證。

### 發現

| # | sha、檔:行 | 觸發情境 | 嚴重度／信心 | 建議修法 | issue |
|---|---|---|---|---|---|
| 1 | `c6dba5659`、`fb69cb281`、`1eeca5e9e`；`daemon/src/runners/ledger_reminders.rs:123`、`crates/am-base/src/ledger.rs:830`、`:677`；`web/src/components/MessageLedgerBar.tsx:103-112`、`web/src/api/ledger.ts:39` | 網頁的「加入追蹤」（提示「交管理者整理」）、「不知道」（下一輪請管理者複查）、「待驗收」（管理者交件）都要管理者動手，但 `scripts/agm.py` 沒有 `ledger` 子命令（0 筆），daemon 的提醒與代班摘要卻寫「請用 `agm ledger claim L<n> --expect-version <v>`」。注入規則與角色提示裡沒有帳本操作說明；`query_actionable_for_actor` 只有測試呼叫。提醒依 `(item, stage, version)` 去重，只發一次。結果：項目停在「待整理」或「進行中」，不會前進。 | 中／高 | 補 `agm ledger`（`list`、`update`／`triage`、`claim`、`handback`、`done`、`cancel`）；提醒每個階段附可執行的指令；CLI 上線前先改掉不存在的指令字樣。 | #2233 |
| 2 | `b1047a48f` `daemon/src/message_translate.rs:578-580`（配 `:226-234`） | 「重翻沒過就保留舊譯文」的判斷是「快取檔裡的譯文不是空字串」，不是「快取是有效命中」。快取裡的譯文若被 `load` 視為未命中（譯文主要是英文，或含假名），每次開啟該訊息都重新打 API（首次＋升級模型重試），沒過也不寫負面標記，下次再打。原文本身含日文假名的訊息另有一個既有問題：通過檢查的譯文存進快取後 `load` 仍當未命中（`c4b1be6c92`，不在這批）。 | 低–中／中（未實測） | `:578` 改用 `load(path).is_some()`；`load` 的假名過濾改成「原文沒有假名才過濾」。 | — |
| 3 | `86949766b` `crates/am-base/src/cache_clock.rs:28`；`docs/SPEC.md:4297` | agy 的快取 TTL 由 720 秒改成 3600 秒，commit 訊息沒有理由；三天前 `5bbb8b01f` 的檔頭寫的是本機 56,246 筆實測「12–16 分 0–12% 命中、超過 30 分 0%」。網頁的「快取約 N 分後到期」對 agy 最多高估 48 分。`SPEC.md:4297` 仍寫 `ttl_secs("agy") = None`。 | 低–中／中 | 需使用者裁示：若 1 小時有依據，補進 `cache_clock.rs` 檔頭與 SPEC；沒有就改回實測值。 | — |
| 4 | `a737c2c7c` `web/src/store/store.ts:557`、`:2674`；`web/src/components/IdentitiesPanel.tsx:165` | `identityLoginPane` 只會寫入，不會清掉；`AddIdentityDialog` 一律拿到 `onOpenTerminal`。先替身份 A 開過背景登入終端，之後新增身份 B 而 B 的終端沒開成功時，對話框的「打開登入終端」會切到 A 的終端（或一個已關閉的 pane）。背景模式遇到既有終端時仍提示「已重新打開」，實際沒有打開。 | 低／中 | 對話框只在 `identityLoginPane` 的 host 與 identity 等於這次建立的身份時才給按鈕；登入完成或 pane 關閉時清掉。 | — |
| 5 | `20a0674a2` `crates/am-lifecycle/src/lifecycle/start.rs:1566-1574` | 受限分享 bot 的啟動分支只套用 overlay 的模型，沒有 `args.extend(overlay.args)`。受限分享 bot 若是 codex，`-c model_auto_compact_token_limit=400000` 不會帶上，但 `policy_rev` 與網頁分母都記成 400K。 | 低／低（未確認受限分享是否允許 codex） | 受限分支也帶 `overlay.args`，或在 `resolve` 對受限＋codex 不回報已套用。 | — |
| 6 | `20a0674a2` `crates/am-config/src/runtime_params.rs:42-51`、`runtime_overlay.rs:22-39` | 分享 bot 預設 400K 之後，有設模型的 claude 分享 bot 啟動時模型一律加 `[1m]`（原本只有明確設定 context 超過 200K 才會）。模型若沒有 1M 版本，啟動可能失敗；有的話會改用長 context 計價。 | 低／低（未實機；不知道各模型是否都有 `[1m]`） | 實機開一顆 haiku 的分享 bot 確認；必要時只對有 1M 版本的模型加。 | — |
| 7 | `d380c3569` `daemon/src/runners/login_assist.rs:135` | agy 登入選單的游標不在「1. Google OAuth」時固定送 `up`＋`enter`，假設游標剛好在它下一列。游標在第三項時會選到第二項（別種登入方式），之後畫面不再是選單，輔助不會修正。 | 低／低（選單預設游標在第一項） | 改成送數字鍵 `1`（若 TUI 支援），或依游標距離送對應次數的 `up`。 | — |
| 8 | `e75cfd00c` `web/src/lib/agyModelGroup.ts:18`、`:72-76` | 同一家族同時有不帶強度的 slug（`gemini-4-neon`）與帶強度的 slug（`gemini-4-neon-high`）時，兩者分組鍵相同：群組同時有 `single` 與 `variants`，點模型按鈕永遠選到 `single`，`agyGroupMembers` 也只回 `single`。 | 低／低（目前 `agy models` 是否會同時列出兩種未確認） | 不帶強度的 slug 另用一個鍵，或併進 `variants` 當預設強度。 | — |
| 9 | `c6dba5659` `web/src/components/MessageLedgerBar.tsx:80-84`、`:117-133` | 「這則不用追蹤」逐項取消後不更新本地狀態，靠 `ledger_updated` 事件重抓；中途失敗時已取消的項目仍顯示。每則使用者訊息下方的「⋯」選單沒有點外面或 Esc 關閉（`24e30af31` 的 `useDismiss` 沒套用）。 | 低／高 | 取消後重抓這則的項目；選單套 `useDismiss`。 | — |
| 10 | `94fecdaab`／`c3e49701c` `web/src/components/TrackingBar.tsx:60-62` | 抓清單失敗（斷線、重啟中）時 `setItems(null)`，整條「追蹤中」列消失，要等下一次帳本事件才回來。 | 低／高 | 失敗時保留上一份清單。 | — |
| 11 | `7e559a4a4` `daemon/src/runners/supervisor/api.rs:696`、`:718-740` | 每小時 12 則的上限以呼叫者計，本機所有排程腳本都算同一個呼叫者 `user`。一小時內不同腳本、不同 reason 合計超過 12 則時，之後的告警回 409 被丟掉。 | 低／中 | `user` 的額度改成依 `source` 分開計。 | — |
| 12 | `814476e0e` `web/src/components/staleBundleBanner.css:69-75` | 深色覆寫只寫在 `@media (prefers-color-scheme: dark)` 裡的 `:root:not([data-theme='light'])`。系統是淺色、網頁手動切成深色（`data-theme="dark"`）時，換版橫幅仍是淺色配色。 | 低／中（未實機） | 比照 `messageLedgerBar.css:146-149` 補 `:root[data-theme="dark"]` 那一組。 | — |
| 13 | `88e23f53b` `web/src/components/QuotaStrip.tsx:424-425` | 沒有任何讀數時一律顯示「查詢中…」，包含查詢已失敗、或該主機不會被探測的情況，文字會一直停在「查詢中…」。 | 低／中 | 有失敗原因或超過一個輪詢週期時改顯示「沒有讀數」。 | — |

**讀過、沒有發現問題的**：`5e7498690`（搬檔，確認框、Bot 綁定檢查、共用身份不可刪都還在）；`8df02e2ca`、`c00748a10`（等價重構）；`0f95cb08b`（每次選取與每次連線各最多一次、進行中不重複）；`687dd9522`；`11a009778`（終端連結只開 http／https）；`60eb85094`、`c56079f55`（`useUpdateChipRequest` 以 `seq` 防重複觸發）；`9dd623538`、`d7af5803a`；`d3880bd24`（Argon 只認週窗、Gemini 只認精確桶 id）；`81d37e282`（只放行 `gemini-pro-orchestrator-` 前綴）；`c83e90007`、`ae511749a`、`5bbb8b01f`；`7e559a4a4` 的 `daemon-update-kick.sh`（`last_deploy.sha_full` 是編進 binary 的 sha，`build_info.rs:122-124`）；`5d738b155`、`2fdc129ea`；`9724d2d26`（「往上讀就先不換」的清單判斷已由 `771b5a03b` 改成只看選取文字，現行檔沒有全文件 `querySelector`）；`fb69cb281` 的 `wait_for` 清空（`agm` 沒有會誤送空字串的呼叫端）；`1eeca5e9e` 的權限（`require_writer`：使用者與專案管理入口；`source_message_id` 會驗專案）。

### 深色主題色碼（14 個 commit）

| commit | 結果 |
|---|---|
| `e4297bd1c`（80 行）、`94fecdaab`（18）、`1eeca5e9e`（4）、`c2ee11a18`（5）、`a923fa66c`（2） | `messageLedgerBar.css` 的寫死色碼都有對應的深色覆寫（`prefers-color-scheme` 與 `data-theme="dark"` 各一組）。 |
| `773c0f3cf`（28） | 已由 `7e559a4a4` 全數改成主題 token，現行檔沒有殘留。 |
| `d8c99894d`（11） | 手機通知膠囊刻意用固定的深色半透明底與淺色字，兩種主題都可讀；error／warn 另有樣式（`styles.css:3421-3445`）。 |
| `fb80eb9e8`（2） | `.quota-expiry-badge` 的 `#111`／`#fff` 疊在 `var(--warn)`／`var(--danger)` 上，兩種主題對比都夠。 |
| `814476e0e`（3） | 見發現 12。 |
| `c3e49701c`（5）、`deeb02ed6`（3）、`2fdc129ea`（2）、`e8eb454bf`（1）、`5e7498690`（1） | 都是 `var(--x, 備用值)` 的備用值或陰影，現行檔沒有獨立的寫死色碼。 |

### 需使用者裁示（新增）

- **agy 快取 TTL 1 小時**（`86949766b`，發現 3）：與同檔三天前記的實測值相反，沒有說明。
- **openai-api 預設模型 astra 的價差**（`663260b48`，前文已列）：補一個數字，astra 輸入 $8、輸出 $40，luna 是 $0.10、$0.50（每百萬 tokens），沒選模型就建立的 openai-api bot 單價是 luna 的 80 倍（`crates/am-config/src/lib.rs:2033`、`openai/pricing.rs:28-71`）。
- **手動完成不經驗收**：管理者 bot 可以直接把項目 PATCH 成 `done`（`patch_item_cas` 不看呼叫者與前一個狀態），「待驗收」只是慣例，不是伺服器強制。若要強制，`answered → done` 只收使用者。

### 仍未覆蓋

- 33 個 commit 的測試、fixture、i18n 字串沒讀；`AddIdentityDialog.tsx:236-482` 的 JSX 沒讀。
- 發現 2、5、6、7、8、12 沒有實測；發現 6 需要實機開一顆分享 bot。
- 窄螢幕版面、深色主題仍沒有在瀏覽器實際操作（色碼只做靜態掃描）。
- §8「只看標題，判定低風險」的 38 個 commit 這次沒有讀 diff（其中 `e4297bd1c`、`814476e0e`、`d8c99894d`、`a923fa66c`、`e8eb454bf` 只看了 CSS 的色碼行）。

## §10 針對性掃描（2026-11-27，唯讀）

行號以 main `b9ee13e20` 為準。開出的 issue：**#2234**（發現 1）。
**第 2、3 項只做了一部分**，範圍與沒看的清單寫在各段與最後的「建議拆法」，不要當成已掃完。

### 覆蓋

| 項目 | 覆蓋 | 說明 |
|---|---|---|
| 1. `290f8af44`、`144ca7fcd` | ✅逐段 | 兩個 diff 全讀（含測試），加上現行 `api.rs:1018-1347`（`auth`、`authorize_bot_path`）、`:2240-2289`、`:3040-3100`、`:3298-3420`、`:3720-3836`、`:5477-5566`，`project_relocate.rs` 全檔，`bot_input.rs:1-75`、`setup.rs:1140-1190`、`projection.rs:128-179`、`hosts.rs:148-150`（`sh_quote`）。沒有實機打 API。 |
| 2. 同型競態 | ⚠部分 | 讀了 `schedules/base.rs`（`lock_latest`、`dispatch`、`clear`）、`agy_session.rs:325-400`（`record_copy`、`stale_copies`、`sweep_copies`）、`start.rs:1630-1670`、`attach.rs:255-432`、`reconcile.rs:905-975`（孤兒 pane）、`remote_purge.rs:22-52`。其餘清理程式沒看，清單見下。 |
| 3. hook 與 poller 不對稱 | ⚠部分 | 讀了 `hookrecv.rs:1505-1584`（`TurnFailed`）、函式清單與副作用呼叫點的對照、`poller.rs:1570-1602`、`:1880-1969`、`turn_error.rs:423-464`、`ask_answers.rs:10-40`。`hookrecv.rs` 的 `TurnComplete` 主體（`:1590-2110`）與 `capture_hookless_turn_locked` 沒逐段讀。 |
| 4. §8 只看標題的 38 個 | ✅分流＋5 個讀 diff | 列出 38 個各自動到的非測試、非 CSS 檔：**沒有任何一個動到 daemon 狀態機、刪除、權限或 delivery**；只有 `77cc819fd` 碰 daemon（`message_translate.rs` 加 8 行測試）。讀了 `77cc819fd`、`4f9886d89`、`d132e8202`、`1c8f97379`、`df663a090` 的邏輯變更行，沒有發現問題。其餘 33 個是 CSS、圖示、排序、測試或刪字串，仍只看檔案清單。 |

### 發現

| # | sha 或檔:行 | 觸發情境 | 嚴重度／信心 | 建議修法 | issue |
|---|---|---|---|---|---|
| 1 | `144ca7fcd` `daemon/src/api.rs:3298-3331`；`crates/am-base/src/bot_input.rs:37-55`；`crates/am-lifecycle/src/lifecycle/setup.rs:1166-1182` | fleet operator 改別的專案、別台主機 bot 的 env 時，保留鍵只擋 `AM_*`，而 bot env 在啟動時蓋過身份的 env 與 daemon 算好的 `PATH`。可設 `CLAUDE_CONFIG_DIR`／`CODEX_HOME`／`GROK_HOME`／`HOME`（等於換身份，繞過 `identity` 的 403，daemon 仍以原身份記額度）、`*_BASE_URL`／`HTTPS_PROXY`（導走 API 流量）、`PATH`／`LD_PRELOAD`／`NODE_OPTIONS`（等於繞過 `args` 的 403）。fleet operator 另有 restart 權限，可立即生效。一般 bot 對 expert 專案的跨專案 prompt 會被擋（`relay_policy.rs:49`），env 是唯一不需目標配合的跨專案路徑。 | 中／高（未實機） | 非 User 的呼叫者另加拒絕清單（身份家目錄鍵、`PATH`、`LD_*`／`DYLD_*`、`NODE_OPTIONS`、`BASH_ENV`、`*_BASE_URL`、`*_PROXY`、`*_API_KEY`／`*_TOKEN`），`env_merge` 的刪鍵也擋；或改白名單。 | #2234 |
| 2 | `290f8af44` `daemon/src/project_relocate.rs:167-171` | 改路徑只改 `deleted_at IS NULL` 的 bot 的 `cwd`。專案搬家後再還原（`agm bot restore`）一顆之前軟刪的 bot，它的 `cwd` 還指向舊路徑。 | 低／中（未實測還原後的啟動行為） | 軟刪的 bot 也一起改，或還原時以專案現行路徑重算。 | — |
| 3 | `290f8af44` `daemon/src/api.rs:1105-1110`、`:1291` | fleet operator 可以把任何專案的路徑改成那台主機上任何已存在的目錄（只擋 `/`、`.`／`..`、同主機其他專案的同一路徑），不檢查是否落在另一個專案的目錄裡、或 AG Man 的資料目錄裡。之後該專案的 bot 就在那個目錄啟動。 | 低／高（屬授權範圍，列出供裁示） | 至少拒絕 AG Man 資料目錄與其子目錄；是否限制巢狀專案由使用者決定。 | — |
| 4 | `144ca7fcd`（既有行為，現在 bot 也走得到）`daemon/src/projection.rs:148-178`、`:376` | 刪 bot 走 `delete_in_config`，這條路不呼叫 `clear_invalid_deputy`（只有 `update_and_project` 會）。被刪的是代班副管理者時，`projects.deputy_bot_id` 與 `/api/state` 的 `deputy` 繼續指向已刪的 bot，直到下一次任何設定寫入。刪的是 expert 專案唯一的 manager 時也沒有檢查，專案變成沒有管理者。 | 低／中 | `delete_in_config` 的閉包裡也清 deputy；刪 manager 回 409 或要求先改模式。 | — |
| 5 | `144ca7fcd`（既有行為）`daemon/src/api.rs:5499-5512` 對 `daemon/src/shell_identity_delete.rs:49-57`、`:103` | 刪設定檔身份只檢查 live bot 的 `bots.identity`；刪 shell alias 那條路另外檢查執行中的 run、巡檢與 `supervisor_roles.identity`。fleet operator（或使用者）刪掉仍被巡檢／responder 角色引用的身份時不會被擋，角色下次啟動找不到身份就不套 env（`setup.rs:1166-1176` 找不到就略過），改用預設帳號。 | 低–中／低（沒有確認角色的 bot 列是否也帶同一個 `identity`） | 兩條刪除路徑共用同一個參照檢查。 | — |
| 6 | 既有 `crates/am-lifecycle/src/lifecycle/agy_session.rs:354-366`；`daemon/src/schedules/base.rs:364-368`、`:401-441` | `sweep_copies` 刪「不是目前基準的 base 複本」。`dispatch` 讀到基準後、`clone_conversation` 複製期間，若使用者重新鎖定或清除基準（`lock_latest`／`clear` 都會丟背景 sweep），舊基準的檔案可能在複製到一半時被刪，這次排程以 `base_fork_failed` 失敗（可重試）。#2232 修的是登記順序，這個窗口還在。 | 低／中（未重現） | `dispatch` 的複製與 sweep 用同一把 per-bot 鎖，或 sweep 略過正在 dispatch 的 bot。 | — |
| 7 | 既有 `agy_session.rs:360-363`；`start.rs:1650` | run 複本的保留條件是「該 bot 最新的 run 複本」或「有執行中的 run 綁著」。分岔啟動在 `record_copy` 之後失敗時，新複本成為最新（沒人用），上一份（bot 實際的最後一段對話）變成可刪，下一次 sweep 會刪掉它；之後一般啟動要接回那段對話就會 `context_lost`。 | 低／低（只影響排程 bot 的手動續聊） | 啟動失敗時把剛登記的 run 複本一併撤掉。 | — |
| 8 | 既有 `crates/am-lifecycle/src/lifecycle/poller.rs:1887`、`:1910-1926`、`:1928-1962` | codex 回合同時有 rollout 錯誤與畫面上的撞限橫幅時，rollout 錯誤那一支先 return，不會執行下面的 `mark_codex_limit_hit` 與 `shadow_limit_hit`。撞限標記要等另一條路（`capture_codex_usage_notices`）補上。 | 低／低（沒有確認另一條路是否一定會補） | rollout 錯誤那一支若同時有 `codex_hit`，也做撞限標記。 | — |
| 9 | 既有 `daemon/src/runners/ask_answers.rs:16-19`；`crates/am-lifecycle/src/hookrecv.rs:587` | 回合內已回答的 `AskUserQuestion` 只在 hook 的 `Stop`／`StopFailure` 處理完之後補記。回合由 poller 的終端備援收尾、而 Stop hook 一直沒到時，這些答案不會被記下。 | 低／中 | 備援收尾後也補讀一次（本機 bot 可讀 transcript）。 | — |

**讀過、沒有發現問題的**（第 1 項）：
- child 與受限分享 bot 走不到這些路由（child 只准讀與對 parent 的 prompt，`api.rs:1037-1039`；受限分享 bot 在 `:1067` 就被拒）；`fleet_project_path`、`fleet_identity_delete` 都要求 `RequestPrincipal::Bot` 且 `is_fleet_operator`（存活的頂層 bot）。
- fleet operator 經 `PATCH /api/projects/{id}` 只能改 `path`（`label`／`handed_off_to` 需 AGM 角色，其餘欄位只限 User）；AGM 角色 bot 不是 fleet operator 就不能改 `path`。
- 刪 bot：不能刪自己、child、分享用 bot、AGM 角色；`?confirm=` 一律不走 fleet 放行；總管專案的 bot 仍由 `guard_bot_delete` 擋。
- 遠端探測的 quoting：路徑先過 `normalize_path`（擋控制字元），再用 `sh_quote`（單引號包起來並跳脫單引號）。
- 前綴改寫：`swap_prefix` 與 SQL 的 `substr` 條件都是「等於舊路徑或以 `舊路徑/` 開頭」，`/a/wt` 不會動到 `/a/wt2`；同主機同路徑有 `projects_host_path_live` 唯一索引擋住併發。
- 刪 bot 後的排程：不刪排程，回應裡列出 `affected_schedules` 並推 `schedules_changed`；分享能力由 `revoke_bot_share` 收回，失敗會背景重試。

**讀過、沒有發現問題的**（第 2 項）：附件清理（`sweep_unreferenced` 用帶同樣條件的 UPDATE 與 `bind` 搶同一列，另有 24 小時門檻）；孤兒 pane（以 `first_seen > ended_at` 與「沒有進行中的 run 用同一個 pane」排除，重啟不重用 pane）；遠端目錄清理的義務用 trigger 寫入，不是兩段寫。

### 沒看的範圍（第 2、3 項）

- 第 2 項沒看的清理程式：`api_bot/retention.rs` 與 `db/api_bot.rs` 的保留清理、`delete_intents`／`intents::sweep_finished` 對 `finish_bot_delete`、`child_retire.rs`（含 trigger）、`bot_trash.rs`／`remote_trash.rs`（還原對清除）、`spawn_hints.rs`、`dead_panes.rs`、`stuck_turns.rs`、`late_proof.rs`、`hook_inbox.rs`、`gone_burst.rs`、`share/remote_fs.rs`、`preview.rs`、`agy_images.rs`、`herdr_maintenance.rs:76`、`ledger::reclaim_orphaned_items` 對副手切換、`transcript_stage.rs`。
- 第 3 項沒看的：hook `TurnComplete` 主體的副作用逐項對照（`background_stop`、`login_prompt` 的授權失敗判斷、`consume_resume_session`、`relay_backfill`、`cache_clock`）在 poller 備援與 `capture_hookless_turn_locked` 是否各有對應；遠端 spool 重播（`replay_spool`）與即時 hook 的差異；codex／grok／agy 各自的 hook 與 poller 分工。

### 建議拆法

兩項都需要逐檔讀完整的函式，一個回合裝不下，建議各拆成獨立派工：

1. **第 2 項**拆三份，各自只看「寫入端的兩段寫」對「清理端的刪除條件」：(a) api_bot 的保留與刪除意圖（`retention.rs`、`db/api_bot.rs`、`delete_intents`、`intents.rs`）；(b) bot 目錄與 child（`bot_trash.rs`、`remote_trash.rs`、`remote_purge.rs` 的 sweep 本體、`child_retire.rs`、`spawn_hints.rs`）；(c) 回合與 pane（`dead_panes.rs`、`stuck_turns.rs`、`late_proof.rs`、`hook_inbox.rs`、`gone_burst.rs`）。
2. **第 3 項**先做一張表：列出 hook `TurnComplete`／`TurnFailed` 收尾時的每個副作用，逐一找 poller 備援（`try_fallback`、`capture_hookless_turn_locked`）的對應呼叫，再依 kind（claude、codex、agy、grok）各看一次。建議一個回合只做 claude＋codex，另一個回合做 agy＋grok 與遠端 spool 重播。

## §11 hook 收尾與 poller 備援的副作用對照（claude＋codex，2026-11-27，唯讀）

行號以 main `5ccf63c2e` 為準。接 §10 第 3 項。**沒有開新的 issue**：找到的缺漏都不到「中以上且信心中以上」。
`StopFailure` 不續行那一項已由 `ec5757901` 修好，不重報。

### 覆蓋

| 範圍 | 覆蓋 | 說明 |
|---|---|---|
| hook 收尾 | ✅逐段 | `hookrecv.rs:541-590`（`process_for`、`after_turn_end`）、`:1340-1591`（共用前段＋`TurnFailed`）、`:1848-2110`（`TurnComplete` 全部分支）、`:1125-1158`（`consume_resume_session`）。 |
| poller 備援 | ✅逐段 | `poller.rs:1552-1607`（備援計時器與其後的補抓）、`:1678-2018`（`try_fallback` 全檔）、`:2100-2283`（`capture_hookless_turn_locked`、`sync_idle_transcript_locked`）；`runners/events.rs:610-660`（誰啟動計時器）。 |
| 各副作用的實作與其他呼叫點 | ⚠讀了入口與呼叫點 | `login_prompt.rs:28-90`、`runners/quota.rs:165-195`、`quota.rs:530-548`、`:990-1004`（撞限怎麼自己清）、`background_hook.rs:1-55`、`compacting.rs:1-110`、`resume_gate.rs:1-135`、`resume_nudge.rs:1-40`、`:170-262`、`codex_session_refresh.rs:1-88`、`interruption.rs` 與 `owed_delivery.rs` 的檔頭、`turn_error.rs:423-520`。各函式的內部沒有全讀。 |
| 沒做 | ✗ | agy、grok、遠端 spool 重播（`replay_spool`）；`grok_transcript::sync_locked` 內部（hookless claude 走它）；沒有任何實機驗證。 |

### 前提：備援什麼時候會接手

- `working → idle` 的狀態邊一定會啟動 5 秒計時器（`events.rs:645-654`、`poller.rs:1545`），**不管回合是不是已經被 hook 收掉**。計時器到了依序做：`try_fallback`（回合還在途才收）、`capture_hookless_turn_locked`、`capture_codex_usage_notices`、`turn_error::capture`（成立就 `api_error_nudge::arm`）。所以「補抓 API 錯誤、撞限、codex 用量提示」這三件事兩條路都會做。
- 真正只走備援收尾的情況：claude 的 Stop hook 沒到（遠端 spool 延遲、payload 被截斷）；**codex 的 Stop hook 還沒被信任**時每一回合都是（log 會有 `codex reply recovered from rollout`、`hook_missing = true`）。下表的缺漏對 codex 是常態，對 claude 是偶發。

### 對照表

「備援」欄：✅＝有對應；◐＝另一條路會補，但較慢或條件不同；✗＝沒有。

| # | hook 收尾做的事（位置） | kind | 備援的對應 | 判定 |
|---|---|---|---|---|
| 1 | 回合收成 `completed`／`failed`、寫回覆、推 `message_added`／`turn`（`:1991-2043`、`:1571-1584`） | 兩者 | `try_fallback` 收成 `completed_fallback`（codex 有 rollout 回覆時是 `completed`）、寫回覆、推同樣的事件（`poller.rs:1886-2017`） | ✅ |
| 2 | 記 native session／turn id、`transcript_path`（`:1875-1883`、`native_evidence`） | 兩者 | codex：`codex_session_refresh` 補 `native_session_id`、rollout 給 turn id（`:1901-1912`）。claude：沒有來源，等遲到的 hook 由 `fill_or_drop_late_hook` 補 | ◐（claude 的 hook 永遠不到就沒有 session id，之後無法 `--resume`） |
| 3 | `compacting::end`（`:1399-1403`） | claude | 沒有。畫面來的標記由 `observe_screen` 收；**hook 的 `PreCompact` 開的標記只有 hook 能收**，否則等 10 分鐘過期（`compacting.rs:16`、`:59-82`） | ✗ 低（見發現 1） |
| 4 | `background_stop`：依 Stop payload 的 `background_tasks` 更新背景工作數（`:1460-1464`） | claude | 畫面判讀 `background_jobs::observe`（`runners/update_watch.rs:100`） | ✅（來源不同） |
| 5 | `login_on_auth_failure`：回覆是「沒登入／401」就標該身份要登入、推播、立刻重驗（`:1466-1469`、`:1516-1518`） | 兩者 | 沒有。`turn_error::capture` 不看授權失敗；只剩例行的身份登入探測 | ✗ 低–中（見發現 2） |
| 6 | `login_on_turn_ok`：正常答完就清掉「要登入」標記（`:1470-1472`） | 兩者 | 沒有。標記要等身份重驗成功才清（`login_prompt::clear_and_push`） | ✗ 低（同發現 2） |
| 7 | `schedule_codex_notice_capture`（`:1476-1479`） | codex | 計時器直接呼叫 `capture_codex_usage_notices`（`poller.rs:1592`） | ✅ |
| 8 | `clear_limit_hit_for_bot`：有回覆就清這顆 bot 的撞限（`:1480-1482`） | codex | 沒有。撞限只會在到期時（`quota.rs:544`、`:1001`）或新的額度讀數校正時清掉 | ✗ 低（見發現 3） |
| 9 | 撞限標記：`StopFailure` 的原因是額度用完就 `mark_claude_limit_hit`（`:1520-1527`） | claude | `turn_error::capture` 讀畫面後同樣標記（`turn_error.rs:455-463`）；codex 走 `fallback_hit`＋`mark_codex_limit_hit`（`poller.rs:1928-1959`） | ✅ |
| 10 | `settle_interruption(Nothing)`、`settle_owed_deliveries`（`:1493-1496`） | 兩者 | 備援不呼叫，但兩者都有定時重試、下一則 prompt 與 reconcile（`reconcile.rs:148`）會結清，檔頭明寫 hook 只是其中一條路 | ◐（不算缺漏） |
| 11 | `consume_resume_session`：比對回報的 session 與要接回的，不符就 `context_lost`，相符就 `verified`，然後 flush 佇列（claude 在 `SessionStart` `:1812`；codex 在第一個 `Stop` `:1855-1858`） | 兩者 | claude：`resume_gate` 等 120 秒後記 `unverified` 並留說明（`resume_gate.rs:25-38`）。codex：`codex_session_refresh` 只在 `native_session_id` 是空的時候寫入，**不比對 `resume_session_id`**（`codex_session_refresh.rs:6-9`、`:62-76`） | claude ◐；codex ✗ 低–中（見發現 4） |
| 12 | 外部回合補使用者訊息並標 `relay_from`（別的 bot 打進來的字，`:2013-2029`、`:2094-2098`） | 兩者 | `begin_external_turn` 用畫面回音開回合，`poller.rs` 沒有任何 relay 來源判斷 | ✗ 低（見發現 5） |
| 13 | `after_turn_end`：補記回合內已回答的 `AskUserQuestion`（`:587`、`ask_answers.rs:16-19`） | claude | 沒有 | ✗ 低（§10 發現 9，不重列） |
| 14 | codex 空回覆的 Stop：不收回合，改 `arm_progress` 讓輪詢接手（`:1908-1917`、`:1969-1979`） | codex | 本來就是輪詢的事 | 不適用 |
| 15 | `StopFailure` 後續行（`after_api_stop_failure`，`:1586-1589`） | claude | `turn_error::capture`＋`api_error_nudge::arm`（`poller.rs:1595-1596`） | ✅（`ec5757901`） |

反方向（備援做、hook 不做）：

| # | 備援做的事 | hook 路徑 | 判定 |
|---|---|---|---|
| 16 | `remember_pane_cursor`：記下讀到畫面的位置（`poller.rs:1919`、`:1940`、`:1971`、`:2010`） | hook 收尾不前移游標 | ◐ 下一回合的備援優先用 prompt 回音定位，另有「跟上一則回覆幾乎相同就不存」的防護（`:1842-1859`）；沒有發現實際問題 |
| 17 | `runs.turn_error`、系統訊息、`shadow_limit_hit`（`:1913-1918`、`:1943-1951`） | hook 的 `TurnFailed` 只寫系統訊息；`turn_error` 由 5 秒後的計時器補 | ✅（有先後差，最多 5 秒） |
| 18 | codex `/compact` 回合的壓縮說明（`:1876-1882`、`:1964-1968`） | hook 走第 14 列，交給輪詢 | ✅ |

### 發現

| # | 檔:行 | 觸發情境 | 嚴重度／信心 | 建議修法 |
|---|---|---|---|---|
| 1 | `crates/am-lifecycle/src/lifecycle/poller.rs:1886-1899`；`crates/am-base/src/compacting.rs:54-82` | claude 的 `PreCompact` hook 到了、之後的 `Stop`／`SessionStart(compact)` 沒到，回合由備援收掉。hook 開的「壓縮中」標記沒有人收，網頁最多多顯示 10 分鐘「壓縮中…」。只影響顯示。 | 低／高 | `try_fallback` 收掉回合（`Outcome::Applied`）之後呼叫 `compacting::end` 並推 `bot_status`。 |
| 2 | `crates/am-lifecycle/src/hookrecv.rs:1466-1474`；`crates/am-lifecycle/src/turn_error.rs:424-463` | 回合由備援收尾、回覆是授權失敗（claude 的沒登入那一行、codex 的 401 或 refresh token 失效）：不會標「這個身份要登入」、不推播、不立刻重驗，要等例行的登入探測。反過來，hook 標過之後若之後的回合都走備援，正常答完也不會清掉標記。codex 的 Stop hook 未信任時每次都是這樣。 | 低–中／中（沒查例行探測的週期，也沒實機） | 備援寫入回覆後用同一支 `is_auth_failure_line` 判斷，呼叫 `login_on_auth_failure`／`login_on_turn_ok`（需要 host fence，可沿用 `client.fence`）。 |
| 3 | `crates/am-lifecycle/src/hookrecv.rs:1480-1482`；`crates/am-lifecycle/src/lifecycle/poller.rs:1988-2017` | codex 撞限之後又正常答完一回合，但回合是 rollout 備援收的：撞限標記不清，排隊的派工與自動挑選繼續避開這顆 bot，直到重置時間到或下一次額度讀數把它校正掉。 | 低／中 | `try_fallback` 存下 codex 的非空回覆後呼叫 `clear_limit_hit_for_bot`（rollout 回覆才清，畫面刮來的不清）。 |
| 4 | `crates/am-lifecycle/src/lifecycle/codex_session_refresh.rs:6-9`、`:62-76`；`crates/am-lifecycle/src/hookrecv.rs:1125-1158` | codex 以 resume 啟動、Stop hook 不會到（未信任）：`runs.resume_session_id` 一直留著、`resume_outcome` 一直是空的；`codex_session_refresh` 補上的 `native_session_id` 即使跟要接回的不同，也不會留 `context_lost` 說明。使用者不會知道接回的不是原本那段對話。 | 低–中／中（沒有確認 codex 接不回時是報錯還是默默開新對話；若一律報錯，這項影響就只剩 `resume_outcome` 不會有結論） | `refresh_inner` 寫入 `native_session_id` 成功後，若 `run.resume_session_id` 有值就呼叫 `consume_resume_session`。 |
| 5 | `crates/am-lifecycle/src/lifecycle/poller.rs:297-398`（`begin_external_turn`）對 `crates/am-lifecycle/src/hookrecv.rs:2020-2026` | 別的 bot 打進這個 pane 的字開了外部回合，而這一回合由備援收尾：使用者訊息沒有 `relay_from`，在網頁上顯示成人打的字（未讀、帳本收件等以「人打的字」判斷的地方也會算進去）。遲到的 hook 到了也只補回覆，不補來源。 | 低／中（沒有查 `fill_or_drop_late_hook` 是否會補 `relay_from`） | `begin_external_turn` 寫使用者訊息時也查 `relay_source`。 |

### 讀過、判定不是缺漏的

- **API 錯誤補抓、撞限、codex 用量提示**：由 `working → idle` 的計時器統一做，不依賴回合是誰收的（表 7、9、15、17）。
- **中斷與送達結果的補記**：hook 是幾條結清路徑之一，另有定時重試與下一則 prompt（表 10）。
- **claude 的 resume 驗證**：hook 不到時 120 秒後記 `unverified` 並在對話留說明，是文件寫明的退路（表 11）。
- **背景工作數**：hook 與畫面各有來源（表 4）。

### 仍未覆蓋

- agy、grok、遠端 spool 重播與即時 hook 的差異。
- hookless 的 claude（`adopted` 且沒注入 hook）走 `grok_transcript::sync_locked`，這支沒有讀。
- 發現 2、3、4、5 都沒有實機重現；發現 4 需要先確認 codex 在 session 接不回時的實際行為。

## §12 hook 收尾與 poller 備援的副作用對照（agy＋grok）與 spool 重播（2026-11-27，唯讀）

行號以 main `5595ee4f6` 為準。接 §11。開出的 issue：**#2237**（發現 1）。其餘不到門檻，或是文件已寫明接受的風險。

### 覆蓋

| 範圍 | 覆蓋 | 說明 |
|---|---|---|
| hook 分類與 agy／grok 專屬處理 | ✅逐段 | `hookrecv.rs:280-330`、`:395-540`（`classify` 的 codex／grok／agy）、`:1014-1121`（`confirm_agy_pre_invocation_delivery`）、`:1161-1237`（`recent_fallback_turn`、`grok_success_at`、`fired_within`）、`:1592-1661`（agy `StatusLine`）、`:1806-1847`（`Identity`）；共用的 `TurnComplete`／`TurnFailed` 在 §11 已讀。 |
| poller 的 agy／grok 分支 | ✅逐段 | `poller.rs:1626-1677`（`agy_hook_recent`、`agy_native_from_transcript`）、`try_fallback` 的 agy／grok 分支（§11 已讀全檔）、`grok_transcript.rs:200-569`（`sync_locked`、`import`、`settle_in_flight`）、`agy_busy.rs` 全檔、`agy_session.rs` 的 `load`、`screen.rs:32-103`（`capture_codex_usage_notices`）、`turn_error.rs:880-905`、`stuck_turns.rs:186-206`。 |
| spool 重播 | ✅逐段 | `hookrecv.rs:2206-2262`（`drain_remote_locked`）、`:2367-2382`（`status_body`）、`:2537-2609`（`replay_spool_locked`）、`setup.rs:40-110`（遠端 `hook.sh` 前段）、`agy_remote.rs:60-135` 與 `agy_hook.rs:23-49`（agy 的遠端與本機 dispatcher）、`events.rs:610-660`（狀態邊觸發 drain）、`hook_inbox.rs` 的 `Source` 與排序。`claim_script`／`fold_spool` 的檔案折疊細節沒讀。 |
| 身份登入標記 | ✅ | `runners/login_prompt.rs:28-90`、`runners/agy_auth.rs` 全檔、`quota_agy.rs` 的冷卻常數。 |
| 沒做 | ✗ | 沒有任何實機驗證；`agy_screen::is_busy`、`limit_banner.rs`、`grok_limit_window` 內部沒讀；grok 的 hooks 安裝腳本（`grok-hook.sh`）沒讀。 |

### 前提

- agy 與 grok 都有 hook。只有「被接管（`adopted`）且沒注入 hook」的 run 才走對話檔同步（`grok_transcript.rs:251-253`）；一般由 AG Man 啟動的 bot，備援就是 `try_fallback` 讀畫面（agy 另讀對話檔取回覆）。
- §11 的前提照舊：`working → idle` 一定啟動 5 秒計時器，之後 `capture_codex_usage_notices`（codex、grok）與 `turn_error::capture` 都會跑。

### 對照表（agy）

| # | hook 做的事（位置） | 備援的對應 | 判定 |
|---|---|---|---|
| 1 | 回合收成 `completed`／`failed`、寫回覆與事件 | `try_fallback`：先讀對話檔（`AgyNative::Reply`），沒有才讀畫面；撞限走 `agy_hit` 收成 failed（`poller.rs:1704-1735`、`:1809`） | ✅ |
| 2 | `agy_busy::clear`（`:1455-1458`）、`fullyIdle:false` 記 `mark_not_idle`（`:1500-1506`） | 證據綁「run＋turn」，回合被備援收掉後不影響下一回合（`agy_busy.rs:122-138`）；備援與閒置收尾都先問 `should_hold` | ✅（不需要對應） |
| 3 | `StatusLine`：寫 `native_session_id`、`agy_busy::note_statusline`、權限確認時把 run 改成 `blocked` 並在解除後 flush 佇列、寫 `status_json`、`cache_clock::on_statusline`、`adopt_statusline_model`（`:1592-1661`） | 沒有 statusLine 時：`agy_session::load` 綁對話並由 `record_status` 從對話檔估 `status_json`（`agy_session.rs:403-419`）；權限框靠畫面判讀（`agy_screen`） | ◐（狀態列不到時，模型與 context 是估的，文件已寫明） |
| 4 | `Identity`（`SessionStart`／`PreInvocation`）：`consume_resume_session`（`:1811-1813`） | 有 hook 的 agy：沒有對應（`agy_session::load` 只綁對話，不比對）。hookless：`sync_locked` 會呼叫（`grok_transcript.rs:265`） | ✗ 低（同 §11 發現 4 的形狀；`resume_gate` 對 agy 不等待） |
| 5 | `PreInvocation` 帶 `lastUserInput` 時補 `delivery_verified`（`:1039-1121`，只限本機） | 備援收尾時有畫面回音或對話檔回覆就設 `delivery_verified`（`poller.rs:1905-1908`） | ✅ |
| 6 | `TurnFailed`：授權失敗 → `login_on_auth_failure`（`:1516-1518`） | 沒有。額度探測看到 `Authentication required` 會翻旗標（`quota_agy.rs` 的 `probe_identity`），是另一條路 | ◐；**hook 這一側本身有錯，見發現 1** |
| 7 | `TurnFailed`：額度用完 → `mark_agy_limit_hit`（`:1520-1527`） | `agy_hit`＋`mark_agy_limit_hit`（`poller.rs:1952-1953`），以及 `turn_error::capture`（`turn_error.rs:455-459`） | ✅ |
| 8 | `TurnComplete`：沒有 statusLine 時用 `lastInputTokens` 寫 `status_json`（`:1860-1873`） | `record_status` 從對話檔算 | ✅ |
| 9 | 外部回合的使用者訊息與 `relay_from` | `begin_external_turn` 不標來源；hookless 的 `import` 會標（`grok_transcript.rs:440-443`、`:501-503`） | ✗ 低（§11 發現 5） |
| 10 | 撞限後成功就清撞限 | hook 與備援**都不清**（hook 只對 codex、grok 清）；agy 靠額度探測的讀數 | 一致 |

### 對照表（grok）

| # | hook 做的事（位置） | 備援的對應 | 判定 |
|---|---|---|---|
| 11 | 回合收尾、寫回覆 | `try_fallback` 讀畫面；撞限走 `grok_limit_notice_lines`＋`mark_codex_limit_hit`（對 grok 會轉成 `Banner::Grok`，`turn_error.rs:893-904`） | ✅ |
| 12 | 撞限標記 | hook **不標**（只避免誤清）；由計時器的 `capture_codex_usage_notices` 讀畫面標（`screen.rs:39-41`、`:86-96`） | ✅（兩條路都靠畫面） |
| 13 | `clear_limit_hit_for_bot_if_older_than`：撞限之後才送出的回合成功收尾就清（`:1485-1491`、`:1183-1202`，只限本機） | 沒有 | ✗ 低（同 §11 發現 3） |
| 14 | `consume_resume_session`（第一個 `Stop`，`:1855-1858`） | 有 hook 的 grok：沒有。hookless 的 `sync_locked` 對 grok **也不呼叫**（只有 agy 分支呼叫，`grok_transcript.rs:263-281`） | ✗ 低–中（同 §11 發現 4） |
| 15 | `Identity`（`session_start`）：寫 `native_session_id`、`transcript_path` | 有 hook 的 grok 沒有其他來源；hookless 由 `session_for` 綁（`:207-237`） | ◐（hook 永遠不到就沒有 session id） |
| 16 | 授權失敗 | hook 與備援**都不處理**（`login_on_auth_failure` 對 grok 直接 return，`login_prompt.rs:43-45`） | 一致 |
| 17 | 外部回合的 `relay_from` | 同第 9 列 | ✗ 低 |

### spool 重播與即時 hook 的差異

即時 hook（HTTP）與重播（本機 spool、遠端 spool）**都先進 `hook_inbox`，再由同一支 `process_for` 處理**，`Source` 只是標籤。所以沒有「重播漏做某個副作用」；差異只在時間與順序：

| # | 差異 | 位置 | 影響 |
|---|---|---|---|
| R1 | 延遲：遠端在 `working → idle`／`→ blocked` 的狀態邊先 drain（有時間上限），之後才啟動備援計時器；沒趕上的要等 30 秒一輪的掃描 | `events.rs:628-635`、`:654`；`hookrecv.rs:2161` | Stop 那一行若在 drain 之後才寫進 spool，備援會先收掉回合，Stop 以「遲到的 hook」處理（見發現 2） |
| R2 | 順序：遠端 claude 的 statusLine 是單槽檔，drain 時**同步、先於**同一批 spool 事件處理，而且 `received_at` 用的是 drain 當下 | `hookrecv.rs:2249-2260`、`:2367-2382` | 只影響額度與狀態列顯示；沒有發現問題 |
| R3 | `status_body` 寫死 `provider: "claude"` | `hookrecv.rs:2376` | 單槽檔只有 claude 的 statusLine 會寫（`setup.rs:70-77`）；遠端 agy 的 `state` 走 spool 佇列、provider 是 agy（`agy_remote.rs:118-125`），不受影響 |
| R4 | 依賴「現在」的判斷：權限提示超過 120 秒就不處理；`grok_success_at`、`confirm_agy_pre_invocation_delivery` 只認本機 | `hookrecv.rs:551`、`:1192-1194`、`:1046` | 遠端 grok 成功回合不清撞限、遠端 agy 不補 `delivery_verified`，都是文件寫明的限制 |
| R5 | 遲到的 Stop 與「120 秒內被備援收掉的回合」比對；超過 120 秒就不比 | `hookrecv.rs:1161-1171` | 主機斷線或 daemon 停機超過 2 分鐘後才重播的 Stop，會被當成外部回合另存一份回覆（原回合留著備援那份），或落到發現 2 的情況 |
| R6 | 本機 daemon 重啟後，新進來的即時 hook 可能比 spool 裡較舊的事件先進收件匣 | `hookrecv.rs:2556-2609`；`hook_inbox.rs:286` | 同 R5，較舊的 Stop 會在較新的回合開始之後才被處理 |

### 發現

| # | 檔:行 | 觸發情境 | 嚴重度／信心 | 建議修法 | issue |
|---|---|---|---|---|---|
| 1 | `daemon/src/runners/agy_auth.rs:15-41`；`daemon/src/runners/login_prompt.rs:35-42` | 綁非預設身份（例如 ag1，自己的 `HOME`）的 agy bot 回合以授權失敗收尾：`on_turn_auth_failure` 不看 bot 的身份，一律把主機**預設帳號**標成未登入、清預設帳號的額度、讓它的探測冷卻 5 分鐘、推播「預設帳號被登出」；真正失效的 ag1 沒有被標記，要等 10 分鐘一輪的身份重驗。claude／codex 的同一條路會先解析身份（`login_prompt.rs:46-62`）。 | 中／中（未實機；沒有確認身份與預設帳號共用 `HOME` 時的期望行為） | 先解析 bot 生效的身份；不共用預設帳號的身份改走 `record_identity_login_fenced` 並清該身份的額度 key。 | #2237 |
| 2 | `crates/am-lifecycle/src/hookrecv.rs:1945-1964`、`:1982-1991` | 遲到的 Stop 到達時已有**下一個**回合在途：只有在「120 秒內有被備援收掉的回合」**且** hook 帶得出使用者訊息原文時，才會改補回上一回合；否則直接把在途的回合收掉、把上一回合的回覆記在它底下。grok 的 Stop 不帶使用者訊息（`classify` 的 `user: None`），所以一定走後者；codex 新版 Stop 也不帶。下一回合真正的 Stop 之後變成另一個外部回合。重播（R1、R5、R6）讓這個時間差更容易出現。 | 中／中，但 **`docs/SPEC.md:4211` 已寫明是接受的殘餘風險**（grok 的 `promptId` 沒在送出時存下來），所以不開 issue | 若要收斂：本機 bot 可加一條「hook 的 `received_at` 早於在途回合的 `delivered_at` 就不認領」（`grok_success_at` 已經在比同一件事）。遠端因兩台時鐘不同，只能比到秒。 | —（文件已接受） |
| 3 | `crates/am-lifecycle/src/lifecycle/grok_transcript.rs:263-281` | hookless 的 grok（被接管、沒注入 hook）以 resume 啟動：`sync_locked` 只有 agy 分支呼叫 `consume_resume_session`，grok 分支綁了 session 也不比對，`resume_outcome` 不會有結論，接回的不是原本那段也不會留 `context_lost`。 | 低／中（被接管的 run 通常不是 AG Man 以 resume 啟動的，實際觸發機會小） | grok 分支取得 `sid` 後同樣呼叫 `consume_resume_session`。 | — |
| 4 | `crates/am-lifecycle/src/lifecycle/agy_busy.rs:122-126`；`stuck_turns.rs:197-203`；`poller.rs:1714-1717` | agy 最後一筆 statusLine 顯示忙碌（`task_count > 0` 或 `agent_state` 是工作中）之後沒有再收到新的 statusLine：`holds` 沒有時效，備援與閒置收尾都不收回合，回合一直在途，直到 run 結束或下一筆 statusLine。 | 低／低（需要 statusLine 事件遺失；本機有 spool、遠端走佇列，正常不會掉） | `LiveState` 記收到時間，超過一段時間（例如 `AGY_HOOK_QUIET` 的 180 秒）且畫面不忙就不再擋。 | — |

§11 的發現 3（撞限後成功不清）、4（resume 不比對）、5（`relay_from`）在 grok 與 agy 也成立，見表中第 4、9、13、14、17 列，不另列。

### 讀過、判定不是缺漏的

- **grok 撞限標記**：hook 本來就不標，兩條路都靠計時器讀畫面（表 12）。
- **agy 撞限後清除**、**grok 授權失敗**：hook 也不做，兩邊一致（表 10、16）。
- **`agy_busy` 的 `fullyIdle:false` 證據**：綁回合 id，不需要備援去清（表 2）。
- **重播的處理程式**：與即時 hook 是同一支，沒有只在其中一邊做的副作用。
- **遠端 agy 的 statusLine**：走 spool 佇列，不經寫死 `claude` 的單槽檔（R3）。

### 仍未覆蓋

- 沒有任何實機驗證；發現 1 需要一顆綁非預設身份的 agy bot 實測。
- `claim_script`、`fold_spool` 的檔案折疊與 `.claim` 卡住的處理沒讀。
- grok 的 hook 安裝腳本與 `limit_banner.rs`、`agy_screen::is_busy` 的內部沒讀。
- hookless 的 claude（`claude_child_log`）沒讀。
