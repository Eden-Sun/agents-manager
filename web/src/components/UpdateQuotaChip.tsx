import { useState } from 'react'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { updateBatchCounts } from '../lib/updateBatch'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import './updateQuotaChip.css'

/**
 * 「claude 有更新」擺在額度列最左邊（SPEC §6.9；位置與外觀 2026-09-11 使用者定）：更新與額度都是 kind 的全域狀態，
 * 側欄收起時也要看得到。對齊量表第一行並補 1px 分隔線（2026-09-11）。按下去先確認：批次重啟沒有取消，確認框是唯一反悔點。
 */
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
  const [confirming, setConfirming] = useState(false)
  // changelog 用第一顆等著套用的 bot 所在主機與它跑著的版本；同一台的 claude 都是同一份。
  const changelogHost = useStore((s) => {
    const first = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready[0]?.botId
    return projectHostName(s, s.bots.find((b) => b.id === first)?.project_id ?? null)
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
      <button
        type="button"
        className={`quota-update running${finished ? ' done' : ''}${failed.length ? ' bad' : ''}`}
        title={finished ? `${summary}（點一下收起，詳細名單在側欄）` : summary}
        aria-label={summary}
        disabled={!finished}
        onClick={finished ? clear : undefined}
      >
        <span aria-hidden="true">{finished ? (failed.length ? '⚠' : '✓') : <UpgradeIcon />}</span>
        <span className="quota-update-n" aria-hidden="true">
          {finished ? ok.length : `${done}/${total}`}
        </span>
      </button>
    )
  }

  if (readyCount === 0 && busyCount === 0) return null

  const busyNote = busyCount > 0 ? `\n${busyCount} 顆在忙，會先跳過：\n${busyLines}` : ''
  const label =
    readyCount === 0
      ? 'claude 有更新，但這些 Bot 現在都在忙——等它們停下來再按'
      : `claude 有更新 · 重啟 ${readyCount} 顆閒置的 Bot（結束目前的 agent，再用同一個 session --resume 接回來，上下文不會掉）`

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
        title="重啟這些 Bot 來套用 claude 更新？"
        body={
          <>
            {confirming ? <UpdateChangelog kind="claude" host={changelogHost} from={changelogFrom} /> : null}
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
