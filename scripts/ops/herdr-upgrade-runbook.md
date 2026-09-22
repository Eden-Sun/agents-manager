# herdr 升級 runbook

這份 runbook 是 AGM 已核准維護窗口後的人工操作步驟。`herdr-update-kick.sh` 只偵測／派工，
不會升級、不會重啟 server；正式操作前仍要取得部署租約、使用者同意與 AGM 排定的維護窗口。

## 為什麼升級後一定要驗本機網路

herdr 是 Homebrew 的 adhoc 簽章，每次升級簽章 identifier 都換：0.8.2＝`herdr-c2fd8e7c703fc4e`，
0.9.1＝`herdr-1672acdeb8e5ac40`。

macOS「本機網路」授權綁 identifier。9/20 升 0.9.1 後，herdr 底下所有非 Apple 程式（node、psql、bun）
連區網都 EHOSTUNREACH，直到 09-22 授權表出現 0.9.1 的項目才恢復。

**驗證陷阱**：Apple 內建的 `/usr/bin/nc`、`/usr/bin/python3`、curl 不受本機網路權限約束、一定會通，
**不能拿來驗**；要用 node（或 Homebrew 的 psql）連區網主機。

授權表是 `/Library/Preferences/com.apple.networkextension.plist`，`plutil -p` 可讀、不用 sudo，
用新版 identifier 或 binary path grep。live-handoff 與 `launchctl submit` 起的全新 server 都**不會**讓授權出現；
要使用者在「系統設定 → 隱私權與安全性 → 本機網路」允許，或在真正的 launchd bootstrap 後按下詢問的允許。

## 升級窗口

1. 確認線上 daemon 已含可同時接受 herdr protocol 20／22 的版本；備份 `~/.config/herdr`（包含 session 檔），
   記下所有正在跑的 session、pane_id 與 bot 名單。保留 0.8.2 binary 作回滾用。
2. 先把新 binary 下載到暫存位置，驗證 `--version`；不要先動線上 server。
3. 窗口開始後，連續完成下列動作：
   - 更新 Homebrew 的 `/opt/homebrew/bin/herdr` symlink。
   - 以暫存檔後 `mv -f` 原子替換 `~/.local/bin/herdr` 等 PATH 優先的私有 copy；不要只升 Homebrew，否則 bot CLI 仍可能是舊版。
   - 對每個 running session 執行 `server live-handoff --import-exe`。在 bot pane 內使用 `env -i`，或先清掉
     `HERDR_SOCKET_PATH`，避免 CLI 打到錯的正式／子 session。

   **不要執行 `server stop`，也不要 bootout launchd job**；它們會殺掉 server 底下的 pane。CLI 與 server 必須在同一個窗口換完，
   因為 protocol 20 與 22 錯位時，雙方向的 herdr CLI 操作都會 `protocol_mismatch`。

   例（session 名稱以當下 `herdr session list` 為準；default 不帶 `--session`）：

   ```sh
   env -i HOME="$HOME" PATH="$PATH" /opt/homebrew/bin/herdr server live-handoff --import-exe /opt/homebrew/bin/herdr
   env -i HOME="$HOME" PATH="$PATH" /opt/homebrew/bin/herdr --session agents-manager server live-handoff --import-exe /opt/homebrew/bin/herdr
   ```

   可先處理低風險 probe／quota，再處理 attach，最後處理 default 與 agents-manager；中間不要把窗口拆開。

4. 每個 session 先驗 `status server` 是新版本／protocol 22／compatible，再驗 `pane list` 的 pane_id 與升級前相同。
   terminal_id 改變是正常的；daemon log 應重新出現 `global herdr event subscription established`。
   launchd KeepAlive 在 handoff 後若短暫以舊快取 job 空轉並回 exit 1，是預期噪音；不要為止噪而 bootout。因為 plist 的
   `ProgramArguments` 走 symlink，symlink 已換好後若 server 崩潰，launchd 接手應會跑新版。

## 升級後強制驗收

必須從 herdr pane 執行，不能在外部 shell 用 Apple 工具代驗：

```sh
bash scripts/ops/herdr-lan-check.sh /opt/homebrew/bin/herdr 192.168.1.1 80
```

這支檢查依序且各自回報：

1. `codesign -dv` 取新 binary 的 identifier。
2. `plutil -p /Library/Preferences/com.apple.networkextension.plist` 查授權表是否有這個 identifier（也回報 path-only 的陷阱）。
3. 使用 node TCP 連 `192.168.1.1:80`（目標可依現場區網替換）。找不到 node 時明確 `SKIP`，不算驗收通過。

任何一步失敗或 skip 都要停下，回報「需要使用者授權本機網路」，請使用者到「系統設定 → 隱私權與安全性 → 本機網路」允許新版 herdr；
不要靠重啟硬試。授權出現後再由同一個 herdr pane 重跑，成功前不可進行後續 pane 驗證。

LAN check 通過後，才做真 pane 的 `agent prompt` 送達、狀態轉換與 `pane read` 驗證。`agent prompt --wait` 的 `stalled` 不等於沒有送達：
它可能已送出並很快完成，只是沒有在等待窗口內觀察到 `working`／`blocked`，不可因此重送。

## 回滾

若 handoff、LAN check 或真 pane 驗證失敗，保留 session-backups 與 `~/.config/herdr` 備份。先把 Homebrew symlink 與 PATH 優先的私有 copy
原子換回 0.8.2，再用 0.8.2 CLI 對每個 session 執行反向 `live-handoff --import-exe`；已驗證 pane_id 與子行程可保留，terminal_id 改變正常。
同樣不要用 `server stop` 或 bootout。若 session 已遺失，依備份與 AGM restore 流程救回 child。
