AGM 定期交辦：正式 daemon（`target/release/agents-managerd serve`，監聽 127.0.0.1:7788）正在執行的 release binary 所含的 commit 落後 origin/main，請重建並重啟套用更新。本任務固定由 AGM 建置 child 執行；不是你申請的 commit 也照固定條件做。

現況（由 bin/daemon-update-kick.sh 自動填寫在本訊息末尾）：執行中 binary 的時間、pid，以及 origin/main HEAD。

固定條件（全部要做到，做不到就不要重啟、直接回報阻塞）：
1. 在乾淨的 HEAD worktree 建 web 與 daemon，不要把別人未提交的 WIP 編進 release；工作樹不要 stash / reset。
2. 整樹 `cargo test` 全過；只含本次 tree 的 `cargo check -p agents-managerd` 與 web `tsc` 通過。
3. 等到**沒有其他 bot 在 working** 再重啟：看 `bin/agm --compact state` 的 `run.agent_status`，只有 `working`
   算數，**`blocked` 不算**（那是在等使用者回答、可能好幾小時，而重啟不動 pane，原 pane 重啟本來就跳過 blocked）。
   不要用 `bin/agm health` 的 `bots.busy`——那個數字把 blocked 也算進去。有人在 working 就等，最多等 30 分鐘，
   超過回報「延後」不要硬重啟。
3-0. **restart 窗口可以在回合內拿**（2026-09-18 起）：`lease acquire restart` 放過申請者自己那顆 bot 的送達臨界區，
   所以不必為了拿窗口把建置或部署腳本丟到背景再結束回合（那違反 6a）。條件：`--owner` ＝核准的 `--requester`，
   `--exclude-bot` 只帶**自己這顆 bot 的 id**；帶別顆會 409 `exclude_not_requester`。別的 bot 還在送達臨界區時照樣拿不到，等它。

3a. **視窗判定與換 binary 要是同一個原子步驟**：輪詢判定「沒人 working」之後，換 binary 前一刻**再查一次**
   `run.agent_status`，仍成立才動手；不成立就回到等待。2026-09-12 22:41 那輪就是判定完到動手的幾秒間有 bot 翻回 working，
   結果在授權範圍外多一顆 bot 在跑時重啟（沒有損失，但那是流程缺陷）。
4. 備份舊 binary 為 `target/release/agents-managerd.bak`。**這批含 DB migration（daemon/src/db.rs、supervisor/、mission/ 的 migrate 有變）時，重啟前也備份正式 DB**：
   `sqlite3 ~/.config/agents-manager/agents-manager.sqlite3 ".backup ~/.config/agents-manager/agents-manager.sqlite3.bak-<YYYYmmdd-HHMM>"`
   （正式檔是 `.sqlite3`；`agents-manager.db`／`am.db` 是 0 byte 空檔，不要備那個），回報路徑與大小。備份後對備份檔跑 `sqlite3 <備份檔> "PRAGMA integrity_check"`（要 `ok`）並記下 `PRAGMA user_version`。**回滾 binary 時 DB 一定要連備份一起還原**（先停 daemon、移走 -wal/-shm、還原後再 integrity_check）：這批若升了 `SCHEMA_VERSION`，只換回 .bak binary 會被版本閘拒絕啟動。細節見 docs/SPEC.md §18.13。
5. 重啟後 30 秒內驗 `/api/session` 與 `bin/agm health`；60 秒內確認 `bin/agm supervisor` 的 status 不是 stopped、`bin/agm state` 沒有 bot 被無故 pane_closed。任一不正常就用 .bak 回滾並回報。
6. 重啟期間不要同時觸發「claude 更新重啟」對其他 bot 動手（已知競態：2026-09-10 23:02Z 把 AGM 等 4 顆 bot 殺掉沒拉回）。

6a. **建置與測試一律在回合內前景跑完，不准丟背景後結束回合。**（2026-09-18 事故）當天 06:09:36 這份任務
   回了一句「正在背景建置測試」就結束回合，背景工作停在 web build 之後沒跑 cargo，沒有任何東西會再叫醒它；
   對 daemon 來說那是一顆 idle 的 bot，90 分鐘後被收起來，交辦停在未結案，kick 每輪跳過，例行部署停了 3.5 小時。
   - `cargo build` / `cargo test` / `bun run build` 都用前景執行，等它印出結果再回報；**不要**用 `&`、`nohup`、
     背景 Bash 工作，也不要「先回報、等一下再看」。
   - 回合結束時每一項驗證都要有數字或結論可貼（passed/failed 數、binary 時間、sha）。還沒有結果就還沒完成。
   - 真的跑不完（建置名額卡住、要等別的 bot、超過額度）就**回報阻塞**並說明還差哪一步，把交辦留在未結案讓 AGM 排，
     不要用「已丟背景」當作完成。

7. 驗證全部通過後，把**這次建進 binary 的 origin/main short sha** 寫進
   `~/.config/agents-manager/supervisor/AGM/daemon-update.built`（一行，`echo <sha> > …/daemon-update.built`）。
   下一輪 kick.sh 就是拿這個 sha 跟 origin/main 比 `daemon/ web/ Cargo.*`，沒有程式碼差異（docs-only）就不會再派重建。
   回滾的話不要寫（binary 還是舊的那顆）。

先看一眼：`git diff --quiet $(cat …/daemon-update.built) origin/main -- daemon web Cargo.toml Cargo.lock`
沒有差異就只做驗證、回報「無需上線」，不要為零程式碼差異中斷使用者與所有 bot。

回報：新 pid、binary 時間、上線的 commit 範圍（舊..新）、AGM 存活確認、有沒有回滾、寫進 .built 的 sha。

## 3b. 上線前對真 herdr 驗（2026-09-14 起，使用者要求）
- build 前 `git -C <checkout> log -1 --format=%h` 必須等於本次要上的 commit；不等就重 fetch 再 build。
- 重啟前用新 binary 對真 herdr 跑 `herdr pane read <任一 pane> --source recent_unwrapped --format ansi`，並送一則短自測 prompt 要 200／delivery ok；任何 409／invalid_request 都不准重啟。
  **自測對象**（2026-09-18 AGM 裁示）：照 3-0 在回合內部署時，對自己送一定回 409 `a turn is already in flight`，那是結構性的、不算數；改送給 AGM 的 browser-gc child（bot `01M248GA4H1TAHJCZRKVR73S3C`），內容一句「[build 自測，回 ok 即可，不要做任何事]」。它剛好在跑例行回合而回 `a turn is already in flight` 就等它結束再送一次；其他 409 照舊不准重啟。
- 重啟後立刻 `bin/agm lease status`：restart 必須 released（daemon 啟動會自動 release）；仍 held 就手動 release，不留保護期。
  **交還窗口要出示 `--lease-token`**（SPEC §18.10）：那把 token 只在 `lease acquire` 的回應裡出現一次，這則交辦的末尾也會附上；
  `lease status` 查不到它。真的拿不到（例如持有者的 pane 已經不在）就**回報請 AGM 代為 force**——`--force` 只有 AGM 角色做得到（你會拿到 403 `lease_force_forbidden`），AGM 做的時候會留稽核紀錄。

## 3c. 核准綁申請者、一張核准一個窗口（3304f1a 起，2026-09-16）
- 這則交辦末尾寫的 **owner 字串**（例如 `daemon-update-kick`）就是 kick 申請 rebuild 核准時的 `--requester`。之後所有 `lease acquire／renew／release` 的 `--owner`
  **都要用同一個字串**，換字串會 409 `approval_owner_mismatch`。
- 一張核准只開一個窗口：租約過期沒 release 再 acquire 會 409 `approval_already_used`，要重新申請核准，不要硬試。
- rebuild 的 lease-token 不再寫在正文，kick 會放在 `~/.config/agents-manager/supervisor/AGM/daemon-update.lease-token`（600 權限）。
- 執行順序固定：
  1. build＋驗證完成後，**先交還 rebuild**：`bin/agm lease release rebuild --owner <owner> --fence <fence> --lease-token "$(cat ~/.config/agents-manager/supervisor/AGM/daemon-update.lease-token)"`。
  2. 再申請 restart 核准：`bin/agm approval request --requester <你自己的 bot id> --purpose restart --scope daemon --commit <sha>`，**不要結束回合**，每 20 秒輪詢最多 10 分鐘等 AGM 裁示。
     （2026-09-18 AGM 裁示：restart 的 requester／owner 一律用**你自己的 bot id**，不是 kick 的腳本字串——3-0 的 `--exclude-bot` 只放過「核准申請者那顆 bot」，腳本身分對不到 bot 會 409 `exclude_not_requester`。rebuild 那段仍用 kick 的 owner。）
  3. 核准後 `bin/agm lease safety --approval <restart 核准> --owner <你的 bot id> --exclude-bot <你的 bot id>` 等窗口，再 `bin/agm lease acquire restart --approval <restart 核准> --commit <sha> --owner <你的 bot id> --exclude-bot <你的 bot id>`；換 binary 前一刻照 3a 再查一次。
  4. 重啟後 daemon 會自動 release restart；仍 held 就用 acquire 回的 token 手動 release（3b）。
- token 檔用完不要刪、不要貼進回報或對話；拿不到就照 3b 請 AGM force 並附理由。
