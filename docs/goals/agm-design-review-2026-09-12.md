# AGM 設計 review — 2026-09-12

結論：daemon 做確定性控制、AGM 做語意判斷的方向合理，已具備持久派工、冪等請求、退避、額度策略與 watchdog。但目前仍是需要人工留意的協作總管，不能把 healthy 或 assignment completed 當作整個系統健康／工作驗收完成。下一步應補狀態與恢復機制，而非繼續加長人設。

## 範圍與證據

- 檢視乾淨來源 ce9f15e（其前兩個提交只改人設／工作規則），未把共用樹 WIP 當成已上線功能。部署紀錄 daemon-update.built 為 e1e34a6。
- 2026-09-12T13:32:23Z 唯讀健康快照：healthy、21 running、3 busy、inbox_open=0；1 筆 queued assignment 的 error=conflict。該筆當時才排隊不到一分鐘，不能據此判為故障。
- 讀 controller、store、health、policy、watchdog、setup、API、派工來源與 runtime 更新腳本，並查實際 assignment 紀錄。
- 用原始 DDL 與 store 查詢在 SQLite :memory: 驗證通知選取行為。未改正式 DB、未發測試工作、未重啟或 build；未執行模型行為實驗或破壞式故障注入。

## P1：回合結束被當成任務完成，缺少驗收狀態

[daemon/src/supervisor/controller.rs:177](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/supervisor/controller.rs:177) 對 completed／completed_fallback turn 直接 finish assignment 為 completed，不判斷回覆是否是中間進度，也不等待 AGM 驗收。[daemon/src/supervisor/store.rs:572](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/supervisor/store.rs:572) 的 open 查詢隨即排除它。

這已有實例：assignment `01M246903Z54XW872GWD7XXJAE` 的 result 是仍在等待編譯（已等 78 分鐘、稍後回報），狀態卻是 completed。這不表示工作最後一定沒做完；能確認的是該狀態不足以證明交付已完成。

影響：未完成工作從待辦消失，例行更新可能把中途回報當成上一筆已結案；新人設的「驗收後結案」沒有程式狀態支援。

建議：保留 turn 的 transport 狀態，assignment 另設 running／blocked／awaiting_review／accepted／failed／cancelled；由 AGM 明確驗收並附證據才 accepted。運維腳本的未結案判斷一併使用這個狀態。

## P1：通知送達後沒有 ACK 恢復路徑，且結案與入 inbox 非原子

[daemon/src/supervisor/controller.rs:306](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/supervisor/controller.rs:306) 的 notify 對任何 Ok(PromptOut) 都標 delivered，沒有判斷 out.delivery；lifecycle 確實存在 Ok(delivery=failed) 的回傳（[daemon/src/lifecycle.rs:3590](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/lifecycle.rs:3590)）。之後 [daemon/src/supervisor/store.rs:688](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/supervisor/store.rs:688) 只選 pending，沒有處理 notify_turn_id 失敗、被中斷或長時間未 ACK 的重排機制。

隔離 SQLite 實驗：同一事件標 delivered、沒有 ACK 後，pending 查詢為 0，open 查詢仍為 1。資料仍在，AGM 主動查 inbox 可補救，但自動喚醒不再處理它；資料庫持久化不等於自動送達保障。

另外 on_turn_done 先 finish assignment 再 push_event；兩步沒有 transaction。daemon 若在中間終止，重啟 reconcile 只查 open assignment，已關閉的那筆不會補出 completion 事件。

建議：完成狀態與 outbox 事件在同一 DB transaction 提交；通知區分 sent、delivery_unknown、handled，依通知 turn 對帳；對明確失敗與過期未 ACK 事件有限退避重送，使用事件 ID 去重，避免無限刷屏。

## P1：健康摘要的覆蓋範圍不足以支撐「全系統巡檢」

[daemon/src/supervisor/health.rs:23](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/supervisor/health.rs:23) 的 severity 主要看 AGM 自己的狀態及 app.connected；其他 Bot 是否持續卡住、host disconnected 數、待辦年齡與資源壓力沒有進 severity。host 數是在 severity 計算之後才取得。

[daemon/src/supervisor/health.rs:74](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/supervisor/health.rs:74) 的 debounce 又只看 severity 與 AGM 自身狀態。因此其他 Bot 掉線或待辦滯留時，UI 計數可能改變，AGM 卻沒有對應健康事件。瀏覽器／dev-server 的獨立排程是局部補足，並非統一系統健康偵測。

建議：將 manager_health 與 system_health 分開；按資源建立 incident（例如 host disconnected、assignment stalled、notify unacked、browser 資源超標），設定持續門檻、去重鍵及恢復事件。保留低噪音設計，但不能把所有非 AGM 異常一起濾掉。

## P2：重建核准與檔案 ownership 仍主要靠對話，沒有執行租約

[daemon/src/supervisor/mod.rs:143](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/supervisor/mod.rs:143) 的派工資料含 target、text、request ID，沒有結構化檔案 ownership、核准 commit、到期時間或部署鎖。supervisor 的 process mutex 保護 API 操作，不能覆蓋外部 Bot 核准後數分鐘的 shell 建置／替換 binary。

runtime 更新腳本雖會查 pending 與 working（[daemon-update-kick.sh:50](/Users/m4p/.config/agents-manager/supervisor/AGM/bin/daemon-update-kick.sh:50)），這仍是一次快照。兩個申請都被允許「等空檔執行」，或讀取之後又有新工作啟動時，沒有共同的 lease 使核准條件在執行期持續成立。

建議：日常申請繼續由 AGM 直接決定，不增加使用者審批。把決定寫成 approval_id（申請者、範圍、commit、期限）；執行前以原子方式取得 rebuild／restart lease，完成或超時釋放。檔案 ownership 也可先做最小的任務範圍衝突記錄。

## P2：四份人設同步解決眼前一致性，未解決版本回退

[daemon/src/supervisor/setup.rs:17](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/supervisor/setup.rs:17) 把 persona 編入 binary，ensure_env 又在 [daemon/src/supervisor/setup.rs:233](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/supervisor/setup.rs:233) 無條件寫回 Bot persona。repo、config、DB、runtime 四份之外，實際還有「binary 內嵌版」及「session 已載入版」。

[daemon-update-kick.sh:31](/Users/m4p/.config/agents-manager/supervisor/AGM/bin/daemon-update-kick.sh:31) 的更新差異只看 daemon／web／Cargo，排除 docs 與 scripts；而 persona 在 docs，CLI 在 scripts，兩者都是 include_str! 的輸入。這次四份已同步，但部署 marker 仍 e1e34a6；若舊 binary 的 setup 再跑，仍可能用舊人設覆蓋新版。這是已知且已告知 AGM 的窗口，不代表目前又被覆蓋。

建議：persona 設 version／hash，以持久設定為運行時權威；setup 只補缺值或執行明確版本遷移，不無條件回填舊 binary 的預設。顯示 stored／loaded／embedded 三種版本；建置依賴判斷納入 include_str! 文件，與是否立刻重啟分開決定。

## P2：手機 Remote 的健康尚未閉環

[daemon/src/supervisor/mod.rs:54](/var/folders/rq/kw6psyqx2q5g6j4brm_sb89h0000gn/T/agm-persona-review-20260912-1vbho9rj/checkout/daemon/src/supervisor/mod.rs:54) 啟動只設 remote=requested；所有 set_remote 呼叫中沒有驗證 active 的觀測者。目前 API 也是 requested、url=null。

影響：這不證明手機現在不能連線，但 daemon 無法確認連線是否正常，更無法可靠偵測入口丟失。AGM 是使用者唯一入口，入口可用性應該是可觀測的服務能力。

建議：增加 provider 支援範圍內的 remote session 觀測（啟用、可用、失效、未知），將 requested 與 verified 分開。無法取得證據時保持 unknown，不假裝在線；支援在不打斷回合的窗口恢復。

## 架構中值得保留的部分

- 10 秒 controller tick 不等於每 10 秒問模型；30 秒 health 採樣與預設 600 秒通知節流分開，方向正確。
- assignment 先持久化、stable client_request_id、unknown delivery 先對帳；Team-managed Bot 不走一般派工，這些邊界有程式支持。
- watchdog 有 desired_running、退避與 5 次失敗上限，避免使用者手動停止後被不停拉起。
- fable <5% 切 opus，返回門檻 20% 加 30 分鐘冷卻，有防止反覆切換的設計；5H／7D 共用額度先判斷，也符合單一帳號的限制。

## 額度與角色的設計限制

fable 與 opus 同屬 cc0，切模型只能避開模型專屬用量，不能避開帳號共用 5H／7D 歸零。這是目前固定候選的能力邊界，不應宣稱已做到獨立備援。兩者都不可用時，daemon 應仍能留存任務、展示異常與等待重置，不依賴 AGM 自己回覆才能知道它失能。

使用者入口與決策總管共用一個 session 在目前規模可以接受，前提是背景計時、去重、鎖與恢復由 daemon 承擔。現階段不建議再開更多總管 session；先補以上三項 P1，否則只是讓多個 session 共同承受同一批狀態缺口。

## 建議落地順序

1. 分開回合完成與任務驗收，補通知 outbox／ACK 對帳。
2. 按資源做 incident 與等待期限，讓 healthy 有明確範圍。
3. 結構化 AGM 核准與部署 lease。
4. 人設版本遷移與 Remote 觀測。

本次僅 review，沒有實作上述修正；P1 標示的是對持久調度承諾的影響，不代表六項都正在正式環境發生事故。
