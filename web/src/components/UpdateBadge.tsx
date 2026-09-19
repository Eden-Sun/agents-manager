import { useState, type ReactNode } from 'react'
import * as api from '../api'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { reviewErrText } from '../lib/claudeReviewErr'
import { updateBatchCounts } from '../lib/updateBatch'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import './updateBadge.css'

/**
 * claude 下載好新版只在 pane 底部印 `Update installed`；daemon 讀成 `runs.update_notice`，這顆 chip 點下去走重啟套用。
 * 2026-09-10：一律先開確認框並列 changelog。`variant="dot"`：側欄 bot 列上的小 ⌃⌃（2026-09-11 使用者：「click to update latest」）。
 * 2026-09-11 使用者：chip 版在批次（`UpdateQuotaChip`）蓋得到時不畫，省標題列第一排寬度；判斷只用 `updateBatch.ts` 與 daemon 同一份規則。
 * 2026-09-19：「蓋得到」含在忙那組——額度列 chip 一樣畫著它。
 */
export function UpdateBadge({ botId, variant = 'chip' }: { botId: string; variant?: 'chip' | 'dot' | 'inline' }) {
  const run = useStore((s) => s.runs[botId] ?? null)
  const botName = useStore((s) => s.bots.find((b) => b.id === botId)?.name ?? '這個 Bot')
  const botKind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? 'claude')
  const host = useStore((s) => projectHostName(s, s.bots.find((b) => b.id === botId)?.project_id ?? null))
  const runningVersion = useStore((s) => s.runs[botId]?.status?.version ?? null)
  const restartBot = useStore((s) => s.restartBot)
  const notify = useStore((s) => s.notify)
  const [restarting, setRestarting] = useState(false)
  const [confirming, setConfirming] = useState(false)
  const [asking, setAsking] = useState(false)

  // selector 回純布林：`updateBatchCounts` 每次回新陣列，連 `useShallow` 都擋不住（見 `UpdateQuotaChip.tsx`）。
  // 忙碌的也算：額度列的 chip 把它列在「在忙」那組，照樣畫著；再畫一顆就是同一個 ⌃⌃ 兩次（2026-09-19 使用者：「logo 重工了」）。
  // 這顆 bot 自己要套用走 context bar 版本號旁的「升級」（`inline`）。
  const coveredByBatch = useStore((s) => {
    const { ready, busy } = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
    return ready.some((b) => b.botId === botId) || busy.some((b) => b.botId === botId)
  })

  const notice = run?.update_notice ?? null
  if (!notice) return null

  const busy = run?.agent_status === 'working' || run?.agent_status === 'blocked'
  const restart = () => {
    setConfirming(false)
    setRestarting(true)
    void restartBot(botId).then((ok) => {
      setRestarting(false)
      if (ok) notify('info', `${botName} 已用新版 claude 重新啟動`)
    })
  }

  // 使用者不想現在重啟，但想知道「這版有沒有我們用得上的東西」：交給 AGM 解析，結論回使用者入口。
  // 唯讀，所以按完就關框，不擋重啟那條路（使用者 2026-09-19）。
  const askAgm = () => {
    setAsking(true)
    void api
      .requestClaudeUpdateReview({ host, from: runningVersion })
      .then((r) => {
        setConfirming(false)
        notify(
          'info',
          // 已經派過（自己按過，或 30 分鐘那支排程先派了）不是錯誤，照實說一次就好。
          r.duplicate
            ? `claude ${r.version} 已經派給 ${r.target_bot_name || 'AGM'} 解析過了，結論會回到這裡`
            : `已請 ${r.target_bot_name} 解析 claude ${r.version} 的 changelog，結論會回到這裡`,
        )
      })
      .catch((e: unknown) => notify('error', `派不出去：${reviewErrText(e)}`))
      .finally(() => setAsking(false))
  }

  const confirmDialog = (
    <ConfirmRestart
      open={confirming}
      name={botName}
      busy={busy}
      asking={asking}
      changelog={confirming ? <UpdateChangelog kind={botKind} host={host} from={runningVersion} /> : null}
      onCancel={() => setConfirming(false)}
      onConfirm={restart}
      onAskAgm={botKind === 'claude' ? askAgm : undefined}
    />
  )

  if (variant === 'dot') {
    return (
      <>
        <button
          type="button"
          className={`bot-update-dot${restarting ? ' busy' : ''}`}
          disabled={restarting}
          aria-label={`${botName}：有更新，點一下套用`}
          title={`${notice}｜點一下先看新版改了什麼，確認後重啟這個 bot（session 會 --resume）${busy ? '\n它正在忙，重啟會打斷這一回合' : ''}`}
          // 疊在 bot 列上：按更新不該順便觸發列的切換。
          onClick={(e) => {
            e.stopPropagation()
            setConfirming(true)
          }}
        >
          <UpgradeIcon size={8} />
        </button>
        {confirmDialog}
      </>
    )
  }

  // `inline`：context bar 版本號右邊那顆（2026-09-12 使用者：「可升級就出現在版本號右邊以方便點選」）；不搶標題列寬度，一律畫。
  if (variant === 'inline') {
    return (
      <>
        <button
          type="button"
          className={`sl-update${restarting ? ' busy' : ''}`}
          disabled={restarting}
          aria-label={`${botName}：${botKind} 有更新，點一下先看新版改了什麼，確認後重啟`}
          title={`${notice}\n點一下先看新版改了什麼，確認後重啟這個 bot（session 會 --resume）${busy ? '\n它正在忙，重啟會打斷這一回合' : ''}`}
          onClick={() => setConfirming(true)}
        >
          {restarting ? '重啟中…' : <><UpgradeIcon size={9} /> 升級</>}
        </button>
        {confirmDialog}
      </>
    )
  }

  // 批次蓋得到就不畫（見檔頭）；但確認框開著或重啟中要留著，否則對話框會被抽走。
  if (coveredByBatch && !confirming && !restarting) return null

  return (
    <>
      <button
        type="button"
        className={`update-badge${restarting ? ' busy' : ''}`}
        disabled={restarting}
        aria-label={`${botName}：${botKind} 有更新，點一下先看新版改了什麼，確認後重啟`}
        title={`${notice}\n點一下先看新版改了什麼，確認後重啟這個 bot（session 會 --resume）${busy ? '\n它正在忙，重啟會打斷這一回合' : ''}`}
        onClick={() => setConfirming(true)}
      >
        {restarting ? '重啟中…' : <><UpgradeIcon /> <span className="update-badge-text">{botKind} 有更新</span></>}
      </button>
      {confirmDialog}
    </>
  )
}

/**
 * 派不出去時要講得出**下一步**。
 *
 * 404／405：這顆 daemon 還沒有這支 API（POST 掉進前端的 catch-all，那條只收 GET 所以是 405——
 * 2026-09-19 使用者實測，當時線上是 a8b84a63、功能還沒上線）。其餘 409 的 `message` 本來就是
 * 「為什麼派不出去」（沒設協調者、沒裝 claude-release-task.md），原樣顯示。
 */
/** 確認框：changelog 在上，忙碌警語在下。 */
function ConfirmRestart({
  open,
  name,
  busy,
  asking,
  changelog,
  onCancel,
  onConfirm,
  onAskAgm,
}: {
  open: boolean
  name: string
  busy: boolean
  asking: boolean
  changelog: ReactNode
  onCancel: () => void
  onConfirm: () => void
  onAskAgm?: () => void
}) {
  return (
    <ConfirmDialog
      open={open}
      title="套用 claude 更新？"
      body={
        <>
          {changelog}
          {busy ? (
            <p>
              <strong>{name}</strong> 正在忙，現在重啟會打斷這一回合。新版 claude 會 <code>--resume</code>{' '}
              接著同一個 session 跑，但這一輪沒說完的話會斷在那裡。
            </p>
          ) : (
            <p>
              重啟 <strong>{name}</strong> 會用新版 claude 接著跑（session 會 <code>--resume</code>，上下文不會掉）。
            </p>
          )}
        </>
      }
      confirmLabel="重啟套用"
      secondaryLabel={onAskAgm ? (asking ? '派工中…' : '請 AGM 解析') : undefined}
      secondaryDisabled={asking}
      onSecondary={onAskAgm}
      danger={busy}
      width={440}
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  )
}
