// issue #1932：agy 並行分擔的 updates 區塊字典（檔案清單見派工）。
import type { Dict } from '../../lib/i18n/types'

const dict: Dict = {
  // toolsHelpers.ts & MergedUpdateChip.tsx & AgyUpdateChip.tsx
  '讀取失敗': 'Could not load',
  '更新': 'Update',
  '知道了': 'Got it',
  '關閉': 'Close',

  // api/deployWait.ts
  '工作中': 'working',
  '正在送訊息或排隊待送': 'Sending message or queued',
  '讀不到狀態': 'status unknown',
  '握著窗口': 'Holding restart window',

  // api/integrationUpdates.ts
  '這顆 daemon 還沒有 integration 更新（HTTP 404），或不認得主機 {host}': 'This daemon does not support integration updates yet (HTTP 404), or does not recognize host {host}',
  '這顆 daemon 還沒有 integration 更新（HTTP 405），二進位比前端舊；要重建並重啟 daemon': 'This daemon does not support integration updates yet (HTTP 405) — binary is older than the frontend; rebuild and restart daemon',
  'integration 更新只給使用者在設定頁按': 'Integration updates can only be triggered by the user in Settings',

  // lib/agyQuotaError.ts
  '探測逾時（agy 一直沒有回應）': 'Probe timed out (agy did not respond)',
  'agy /usage 的輸出讀不懂': 'Cannot parse agy /usage output',
  'agy 執行失敗': 'agy execution failed',
  '這台主機的 herdr pane 開不起來': 'Cannot spawn herdr pane on this host',
  '主機尚未連線': 'Host not connected yet',
  '原因不明': 'unknown reason',

  // lib/onboardingActions.ts
  '已在 {host} 開 shell 安裝 {kind}，裝完會自動重新偵測': 'Opened shell on {host} to install {kind}; will re-check automatically once done',
  '安裝 {kind} 失敗：{reason}': 'Installing {kind} failed: {reason}',

  // components/AgyUpdateConfirm.tsx
  '在 {host} 把 agy 從 {from} 更新到官方最新版（約 {target}）：從官方下載（約 50 MB）、驗 sha512 後換掉 {path}；不跑官方 install.sh、{cmd1} 或 {cmd2}。跑著的 agy bot 不會被打斷，重啟後才換版。':
    'On {host}, update agy from {from} to official latest (approx. {target}): downloads from official source (approx. 50 MB), verifies sha512, and replaces {path}; does not run official install.sh, {cmd1}, or {cmd2}. Running agy bots are not interrupted and will switch version on restart.',

  // components/AgyUpdateChip.tsx
  'agy 更新中…': 'Updating agy…',

  // components/GrokUpdateChip.tsx & lib/grokUpdate.ts
  'grok 有新版 {version}': 'grok has a new version {version}',
  '這些主機的 grok 還不是最新的正式版：': 'grok on these hosts is not yet on the latest stable release:',
  '讀不到版本': 'version unknown',
  '到那台主機執行 {cmd}（grok 官方的升級指令）。這裡不代為安裝；裝好之後跑著的 grok bot 要重啟才會換成新版。':
    'Run {cmd} (official grok upgrade command) on that host. Not installed automatically from here; running grok bots must be restarted to use the new version.',
  'grok 有新版 {target}（{hosts}）· 在那台主機執行 `{command}`，已在跑的 bot 重啟後才會換版':
    'grok has a new version {target} ({hosts}) · run `{command}` on that host; running bots must be restarted to switch versions',
  'grok {target}：{hosts} 還沒升（只提示，指令 {command}）':
    'grok {target}: {hosts} not updated yet (notice only, command {command})',

  // lib/herdrVersion.ts
  'herdr 版本：未知': 'herdr version: unknown',
  '主機沒連上或讀不到 herdr 版本': 'Host offline or cannot read herdr version',
  'herdr CLI {cli}（server 版本未知）': 'herdr CLI {cli} (server version unknown)',
  'CLI 是 {cli}，但跑著的 herdr server 還是 {sv}：bot 的 herdr 指令會回 protocol_mismatch，要重啟 herdr server 才會換到新版。':
    'CLI is {cli}, but running herdr server is {sv}: bot herdr commands will return protocol_mismatch; restart herdr server to switch to the new version.',
  '未驗證': 'Unverified',
  'protocol {protocol} 不在 daemon 實測過的清單，RPC 形狀沒驗過。':
    'protocol {protocol} is not in the daemon-tested list; RPC shape is unverified.',

  // store/integrationAutoInstall.ts
  '已在 {where} 自動安裝 herdr {name} integration': 'Automatically installed herdr {name} integration on {where}',
  '在 {where} 自動安裝 herdr {name} integration 失敗：{error}（不重試；到設定頁「更新檢查」按安裝再試，或重啟 daemon）':
    'Failed to automatically install herdr {name} integration on {where}: {error} (will not retry; click install in Settings Update check, or restart daemon)',

  // api/herdrUpdate.ts
  '這顆 daemon 還沒有 herdr 一鍵更新（HTTP 404），或不認得主機 {host}': 'This daemon does not support herdr one-click update yet (HTTP 404), or does not recognize host {host}',
  '這顆 daemon 還沒有 herdr 一鍵更新（HTTP 405），二進位比前端舊；要重建並重啟 daemon': 'This daemon does not support herdr one-click update yet (HTTP 405) — binary is older than frontend; rebuild and restart daemon',
  '{host} 是遠端主機，這版只支援更新本機的 herdr': '{host} is a remote host; this version only supports updating herdr on this machine',
  '{host} 的 herdr session 跟別的 daemon 共用，不能從這裡重啟': 'herdr session on {host} is shared with another daemon and cannot be restarted from here',
  '要升的版本已經變成 {cur}，關掉框重開一次': 'Target version has changed to {cur}; please close and reopen the dialog',
  '已經沒有等著升級的新版（可能剛升好了）': 'No pending version to upgrade (it may have just finished upgrading)',
  '已經有一次 herdr 更新在跑': 'A herdr update is already in progress',
  '只能從網頁按，Bot 不能自己觸發 herdr 更新': 'Can only be triggered from the web UI; bots cannot trigger herdr updates',

  // components/AgyInstall.tsx
  '安裝 agy？': 'Install agy?',
  '{host} 尚未安裝 agy，要自動安裝嗎？': '{host} does not have agy installed yet. Install automatically?',
  '會從官方下載（約 50 MB）、驗 sha512 後放到 {path}；不跑官方 install.sh、不改 shell 設定。':
    'Downloads from official source (approx. 50 MB), verifies sha512, and puts it in {path}; does not run official install.sh and does not alter shell config.',
  '在{host}從官方下載 agy（約 50 MB、驗 sha512）並放到 ~/.local/bin/agy；不跑官方 install.sh、不改 shell 設定':
    'Download agy on {host} from official source (approx. 50 MB, verifies sha512) and put into ~/.local/bin/agy; does not run official install.sh or alter shell config',
  '安裝 agy': 'Install agy',

  // components/UpdateAllBanner.tsx
  '重啟完成 · 成功 {ok} 顆': 'Restart done · {ok} succeeded',
  ' · 跳過 {skipped} 顆': ' · {skipped} skipped',
  ' · 失敗 {failed} 顆': ' · {failed} failed',
  '重啟中 {progress}': 'Restarting {progress}',
  '{done} 顆（總數未知）': '{done} items (total unknown)',
  '收起這則摘要': 'Dismiss this summary',
  '收起進度（不會中斷重啟）': 'Hide progress (restart will not be interrupted)',

  // components/CodexUpdateHint.tsx & lib/codexUpdatePrompt.ts
  '畫面在這中間換過了，這一下沒有送出去——上面是重讀後的畫面，請再點一次。':
    'The screen changed in the meantime and this action was not sent — screen reloaded above, please click again.',
  '收起 changelog': 'Collapse changelog',
  '先看改了什麼': 'View changelog first',
  '送出 1：現在更新（codex 會自己跑安裝指令）': 'Send 1: update now (codex will run the install command)',
  '1 更新': '1 Update',
  '送出 2：這次跳過，下次還會問': 'Send 2: skip this time, will prompt next time',
  '2 跳過': '2 Skip',
  '送出 3：跳過這一版，出更新的版本才再問': 'Send 3: skip this version, prompt again on newer versions',
  '3 跳過這版': '3 Skip this version',
  '按 1 更新前先看新版的 release notes。': 'View release notes before pressing 1 to update.',
  '目前沒有對應的互動選單，只能查看新版 release notes。': 'No interactive menu currently available; view release notes only.',

  // components/DeletedBotsPanel.tsx
  '剛剛': 'just now',
  '{n} 分鐘前': { one: '{n} minute ago', other: '{n} minutes ago' },
  '{n} 小時前': { one: '{n} hour ago', other: '{n} hours ago' },
  '{n} 天前': { one: '{n} day ago', other: '{n} days ago' },
  '讀不到已刪除的清單，稍後再開一次。': 'Cannot load deleted list; try opening again later.',
  '載入中…': 'Loading…',
  '沒有已刪除的 Bot。': 'No deleted Bots.',
  '刪除是軟的：設定與對話都還在，按「復原」就回到原本的專案。': 'Deletion is soft: settings and messages remain. Click Restore to return to the original Project.',
  '{project} · 刪除於 {ago}': '{project} · deleted {ago}',
  ' · 最後對話 {ago}': ' · last active {ago}',
  '復原': 'Undo',

  // lib/agyUpdate.ts
  '更新 Antigravity（agy）CLI 到 {target}？': 'Update Antigravity (agy) CLI to {target}?',
  'agy 有新版 {target}（{hosts}）· 點一下更新：從官方下載、驗 sha512 後換掉 ~/.local/bin/agy；跑著的 agy bot 重啟後才換版':
    'agy has a new version {target} ({hosts}) · click to update: downloads from official source, verifies sha512, and replaces ~/.local/bin/agy; running agy bots switch version on restart',
  'agy {target}：{hosts} 可更新（點開確認）': 'agy {target}: {hosts} can update (click to confirm)',

  // lib/toolInstallDone.ts
  '{where} 的安裝追蹤已失聯（主機連線或偵測已換代），請重新偵測並確認原安裝程序':
    'Lost installation tracking on {where} (host connection or detection changed); please re-check and verify installer process',
  '{where} 安裝 {kind} 等了 15 分鐘仍未確認結束（逾時）；請檢查保留的安裝終端':
    'Installing {kind} on {where} waited 15 minutes without confirmation (timed out); check the kept terminal pane',
  '{where} 已安裝 {kind}': '{kind} is installed on {where}',
  '安裝結束後仍偵測不到': 'Still not detected after installation finished',
  '{where} 沒裝成 {kind}：{why}；安裝終端留著，錯誤輸出在裡面':
    'Failed to install {kind} on {where}: {why}; install terminal kept with error output',
  '{where} 沒裝成 {kind}：{why}；安裝終端已關閉，請再按一次「安裝」':
    'Failed to install {kind} on {where}: {why}; install terminal closed, please click Install again',

  // lib/deployNow.ts
  '部署中{sha}：建置中或等換版窗口': 'Deploying{sha}: building or waiting for the swap window',
  '{owner}正握著 {resource} 窗口（重建或換版進行中）': '{owner} is holding {resource} window (rebuild or restart in progress)',
  '有人': 'Someone',
  '有部署在跑': 'Deploy in progress',
  '已送出部署 {short}。左上角會顯示「部署中」，換好後自動消失；等太久會跳出等待提示。':
    'Deploy of {short} sent. The top-left shows "Deploying" and clears once swapped; a waiting notice appears if it takes too long.',
  '沒有再開一趟：{msg}': 'Not started again: {msg}',
  '不用部署：{msg}': 'No deploy needed: {msg}',
  '部署已拒絕：{msg}': 'Deploy rejected: {msg}',
  '不能從這裡部署：{msg}': 'Cannot deploy from here: {msg}',
  '立即部署沒有開始：{msg}': 'Deploy now did not start: {msg}',
}

export default dict
