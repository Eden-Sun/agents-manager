import { useState } from 'react'
import * as api from '../api'
import { ApiError } from '../api/types'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { needsManualInstall, updateBatchCounts } from '../lib/updateBatch'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import { AgmReviewBox } from './AgmReviewBox'
import './updateQuotaChip.css'

/**
 * 「claude 有更新」擺在額度列最左邊（SPEC §6.9；位置與外觀 2026-09-11 使用者定）：更新與額度都是 kind 的全域狀態，
 * 側欄收起時也要看得到。對齊量表第一行並補 1px 分隔線（2026-09-11）。按下去先確認：批次重啟沒有取消，確認框是唯一反悔點。
 */
/** 同 `UpdateBadge` 的 `reviewErr`：404／405 ＝這顆 daemon 還沒有那支 API。 */
function reviewErr(e: unknown): string {
  if (e instanceof ApiError && (e.status === 404 || e.status === 405)) {
    return `這顆 daemon 還沒有 /api/claude-update/review（HTTP ${e.status}），二進位比前端舊；要重建並重啟 daemon 才會有這支 API。`
  }
  return e instanceof Error ? e.message : String(e)
}

export function UpdateQuotaChip() {
  const batch = useStore((s) => s.restartBatch)
  const restartIdleBots = useStore((s) => s.restartIdleBots)
  const clear = useStore((s) => s.clearRestartBatch)
  const sending = useStore((s) => Boolean(s.busy['restart-idle']))
  // 各 selector 回純值：`updateBatchCounts` 每次回新陣列，包成物件連 `useShallow` 都擋不住（getSnapshot should be cached）。
  const readyCount = useStore((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.length)
  const readyNames = useStore((s) =>
    updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
      .ready.map((b) => b.name)
      .join('、'),
  )
  const busyLines = useStore((s) =>
    updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
      .busy.map((b) => `${b.name}（${b.why}）`)
      .join('\n'),
  )
  const busyCount = busyLines ? busyLines.split('\n').length : 0
  // 還沒裝新版的 codex（2026-09-25 使用者：codex 有更新 header 也要提示）：灰掉按不動就看不出是提示，
  // 沒有可重啟的時候改成點一下切到那顆 bot（已在其中一顆就換下一顆），在它的徽章看 changelog、裝好再重啟。
  const installIds = useStore((s) =>
    s.bots
      .filter((b) => needsManualInstall(b, s.runs[b.id]))
      .map((b) => b.id)
      .join(' '),
  )
  const selectedBotId = useStore((s) => s.selectedBotId)
  const selectBot = useStore((s) => s.selectBot)
  const [confirming, setConfirming] = useState(false)
  const [asking, setAsking] = useState(false)
  const [reviewKey, setReviewKey] = useState(0)
  const notify = useStore((s) => s.notify)
  // changelog 用第一顆等著套用的 bot 所在主機、kind 與它跑著的版本；同一台同 kind 都是同一份。
  const changelogHost = useStore((s) => {
    const first = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready[0]?.botId
    return projectHostName(s, s.bots.find((b) => b.id === first)?.project_id ?? null)
  })
  const changelogKind = useStore((s) => {
    const first = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready[0]?.botId
    return s.bots.find((b) => b.id === first)?.kind ?? 'claude'
  })
  const changelogFrom = useStore((s) => {
    const first = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready[0]?.botId
    return first ? (s.runs[first]?.status?.version ?? null) : null
  })

  if (batch) {
    const { total, done, ok, failed, finished } = batch
    const summary = finished
      ? `重啟完成 · 成功 ${ok.length} 顆${failed.length ? ` · 失敗 ${failed.length} 顆` : ''}`
      : `重啟中 ${done}/${total}`
    return (
      // 未完成也可以收起（issue #492）：以前 `disabled={!finished}`，`bots_restart_done` 收不到時
      // 這顆會永遠停用，而它又蓋住一鍵重啟的觸發鈕，等於連再按一次都不行。收起不中斷批次。
      <button
        type="button"
        className={`quota-update running${finished ? ' done' : ''}${failed.length ? ' bad' : ''}`}
        title={`${summary}（點一下收起，詳細名單在側欄${finished ? '' : '；不會中斷重啟'}）`}
        aria-label={summary}
        onClick={clear}
      >
        <span aria-hidden="true">{finished ? (failed.length ? '⚠' : '✓') : <UpgradeIcon />}</span>
        <span className="quota-update-n" aria-hidden="true">
          {finished ? ok.length : `${done}/${total}`}
        </span>
      </button>
    )
  }

  if (readyCount === 0 && busyCount === 0) return null

  if (readyCount === 0 && installIds) {
    const ids = installIds.split(' ')
    const next = ids[(ids.indexOf(selectedBotId ?? '') + 1) % ids.length]
    const installLabel = `codex 有新版，要先手動安裝（${ids.length} 顆 Bot）· 點一下切到那顆，在它的標題列看 changelog`
    return (
      <button
        type="button"
        className="quota-update install"
        title={`${installLabel}\n${busyLines}`}
        aria-label={installLabel}
        onClick={() => selectBot(next)}
      >
        <span aria-hidden="true"><UpgradeIcon /></span>
        <span className="quota-update-n" aria-hidden="true">{busyCount}</span>
      </button>
    )
  }

  const busyNote = busyCount > 0 ? `\n${busyCount} 顆在忙（或還沒手動安裝），會先跳過：\n${busyLines}` : ''
  const label =
    readyCount === 0
      ? '有 CLI 更新，但這些 Bot 現在都在忙或還沒手動安裝——處理完再按'
      : `有 CLI 更新 · 重啟 ${readyCount} 顆閒置的 Bot（結束目前的 agent，再用同一個 session --resume 接回來，上下文不會掉）`

  return (
    <>
      <button
        type="button"
        className={`quota-update${readyCount === 0 ? ' waiting' : ''}`}
        disabled={sending || readyCount === 0}
        title={`${label}${readyCount > 0 ? `：\n${readyNames}` : ''}${busyNote}`}
        aria-label={label}
        onClick={() => setConfirming(true)}
      >
        <span aria-hidden="true"><UpgradeIcon /></span>
        <span className="quota-update-n" aria-hidden="true">
          {sending ? '…' : readyCount === 0 ? busyCount : readyCount}
        </span>
      </button>
      <ConfirmDialog
        open={confirming}
        title="重啟這些 Bot 來套用更新？"
        body={
          <>
            {confirming ? (
              <div className="update-split">
                <UpdateChangelog kind={changelogKind} host={changelogHost} from={changelogFrom} />
                {/* AGM 解析目前只有 claude 這條路（/api/claude-update/review 尚未支援 codex）；
                    codex 只顯示 changelog，不冒充 claude 的解析結論。 */}
                {changelogKind === 'claude' ? <AgmReviewBox host={changelogHost} from={changelogFrom} refreshKey={reviewKey} /> : null}
              </div>
            ) : null}
            <p>
              以下 <strong>{readyCount}</strong> 顆會結束目前的 agent，再用同一個 session <code>--resume</code>{' '}
              接回來——上下文不會掉，但重啟要花幾秒，這段時間它們不會回話。
            </p>
            <ul className="confirm-list">
              {readyNames.split('、').map((n) => (
                <li key={n}>{n}</li>
              ))}
            </ul>
            {busyCount > 0 ? (
              <>
                <p className="confirm-note">另外 {busyCount} 顆在忙，這次會跳過：</p>
                <ul className="confirm-list dim">
                  {busyLines.split('\n').map((n) => (
                    <li key={n}>{n}</li>
                  ))}
                </ul>
              </>
            ) : null}
          </>
        }
        confirmLabel={`重啟 ${readyCount} 顆`}
        // 批次框跟單顆框同一顆按鈕：claude 更新的解析是**版本層級**的事，跟要重啟幾顆無關。
        // 單顆的 chip 在批次蓋得到時會自己隱藏，所以只做在 UpdateBadge 上等於多數情況看不到
        // （使用者 2026-09-19：「還是沒見按鈕」）。「請 AGM 解析」目前只有 claude 這條路，見上面
        // `AgmReviewBox` 的註解——codex 的批次沒有這顆按鈕，不是漏做。
        secondaryLabel={changelogKind === 'claude' ? (asking ? '派工中…' : '請 AGM 解析') : undefined}
        secondaryDisabled={asking}
        onSecondary={changelogKind === 'claude' ? () => {
          setAsking(true)
          void api
            .requestClaudeUpdateReview({ host: changelogHost, from: changelogFrom })
            .then((r) => {
              // 框留著：結論直接顯示在裡面。
              setReviewKey((n) => n + 1)
              notify(
                'info',
                r.duplicate
                  ? `claude ${r.version} 已經派給 ${r.target_bot_name || 'AGM'} 解析過了，結論會回到這裡`
                  : `已請 ${r.target_bot_name} 解析 claude ${r.version} 的 changelog，結論會回到這裡`,
              )
            })
            .catch((e: unknown) => notify('error', `派不出去：${reviewErr(e)}`))
            .finally(() => setAsking(false))
        } : undefined}
        width={440}
        onCancel={() => setConfirming(false)}
        onConfirm={() => {
          setConfirming(false)
          void restartIdleBots()
        }}
      />
    </>
  )
}
