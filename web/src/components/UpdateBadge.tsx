import { useState, type ReactNode } from 'react'
import { useReviewAsk } from '../hooks/useReviewAsk'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { useT } from '../lib/i18n'
import { updateRange } from '../lib/updateRange'
import { isChildBot, passiveUpdateHint, updateBatchCounts } from '../lib/updateBatch'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import { AgmReviewBox } from './AgmReviewBox'
import './updateBadge.css'

export function UpdateBadge({ botId, variant = 'chip' }: { botId: string; variant?: 'chip' | 'dot' | 'inline' }) {
  const t = useT()
  const run = useStore((s) => s.runs[botId] ?? null)
  const botName = useStore((s) => s.bots.find((b) => b.id === botId)?.name ?? t('這個 Bot'))
  const botKind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? 'claude')
  const host = useStore((s) => projectHostName(s, s.bots.find((b) => b.id === botId)?.project_id ?? null))
  const runningVersion = useStore((s) => s.runs[botId]?.status?.version ?? null)
  const restartBot = useStore((s) => s.restartBot)
  const notify = useStore((s) => s.notify)
  const [restartingLocal, setRestarting] = useState(false)
  const restartingShared = useStore((s) => Boolean(s.busy[`restart:${botId}`]))
  const restarting = restartingLocal || restartingShared
  const [confirming, setConfirming] = useState(false)

  const coveredByBatch = useStore((s) => {
    const { ready, busy, passive } = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
    return [ready, busy, passive].some((list) => list.some((b) => b.botId === botId))
  })
  const passiveHint = useStore((s) => {
    const bot = s.bots.find((b) => b.id === botId)
    return bot ? passiveUpdateHint(bot, s.runs[botId]) : null
  })

  const isChild = useStore((s) => {
    const bot = s.bots.find((b) => b.id === botId)
    return bot ? isChildBot(bot) : false
  })
  const notice = run?.update_notice ?? null
  const range = updateRange(botKind, notice, runningVersion)
  const review = useReviewAsk(botKind, host, range.from, range.to, false)
  if (!notice) return null
  if (isChild) {
    const hint = passiveHint ?? t('由父 Bot 安排重啟')
    if (variant === 'dot') {
      return (
        <span className="bot-update-dot passive" role="img" aria-label={t('{name}：{hint}', { name: botName, hint })} title={`${hint}\n${notice}`}>
          <UpgradeIcon size={8} />
        </span>
      )
    }
    if (variant === 'inline') {
      return (
        <span className="sl-update passive" title={`${hint}\n${notice}`}>
          <UpgradeIcon size={9} /> {t('由父 Bot 重啟')}
        </span>
      )
    }
    return null
  }

  const busy = run?.agent_status === 'working' || run?.agent_status === 'blocked'
  const canReview = botKind === 'claude' || botKind === 'codex'
  const needsInstall = botKind !== 'claude' && !notice.includes('已安裝')
  const restart = () => {
    if (needsInstall) {
      setConfirming(false)
      return
    }
    setConfirming(false)
    setRestarting(true)
    void restartBot(botId).then((ok) => {
      setRestarting(false)
      if (ok) notify('info', t('{name} 已用新版 {kind} 重新啟動', { name: botName, kind: botKind }))
    })
  }

  const confirmDialog = (
    <ConfirmRestart
      open={confirming}
      kind={botKind}
      notice={notice}
      needsInstall={needsInstall}
      name={botName}
      busy={busy}
      asking={review.asking}
      changelog={
        confirming ? (
          <div className="update-split">
            <UpdateChangelog kind={botKind} host={host} from={range.from} to={range.to} />
            {canReview ? <AgmReviewBox kind={botKind} host={host} from={range.from} to={range.to} refreshKey={review.reviewKey} /> : null}
          </div>
        ) : null
      }
      onCancel={() => setConfirming(false)}
      onConfirm={restart}
      onAskAgm={canReview ? review.ask : undefined}
    />
  )

  const busyNote = busy ? t('\n它正在忙，重啟會打斷這一回合') : ''
  const passivePrefix = passiveHint ? `${passiveHint}\n` : ''

  if (variant === 'dot') {
    return (
      <>
        <button
          type="button"
          className={`bot-update-dot${restarting ? ' busy' : ''}`}
          disabled={restarting}
          aria-label={t('{name}：有更新，點一下套用', { name: botName })}
          title={`${passivePrefix}${t('{notice}｜點一下先看新版改了什麼，確認後重啟這個 bot（session 會 --resume）{busy}', { notice, busy: busyNote })}`}
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

  if (variant === 'inline') {
    return (
      <>
        <button
          type="button"
          className={`sl-update${restarting ? ' busy' : ''}`}
          disabled={restarting}
          aria-label={t('{name}：{kind} 有更新，點一下先看新版改了什麼，確認後重啟', { name: botName, kind: botKind })}
          title={`${passivePrefix}${notice}\n${t('點一下先看新版改了什麼，確認後重啟這個 bot（session 會 --resume）{busy}', { busy: busyNote })}`}
          onClick={() => setConfirming(true)}
        >
          {restarting ? t('重啟中…') : <><UpgradeIcon size={9} /> {t('升級')}</>}
        </button>
        {confirmDialog}
      </>
    )
  }

  if (coveredByBatch && !confirming && !restarting) return null

  return (
    <>
      <button
        type="button"
        className={`update-badge${restarting ? ' busy' : ''}`}
        disabled={restarting}
        aria-label={t('{name}：{kind} 有更新，點一下先看新版改了什麼，確認後重啟', { name: botName, kind: botKind })}
        title={`${passivePrefix}${notice}\n${t('點一下先看新版改了什麼，確認後重啟這個 bot（session 會 --resume）{busy}', { busy: busyNote })}`}
        onClick={() => setConfirming(true)}
      >
        {restarting ? t('重啟中…') : <><UpgradeIcon /> <span className="update-badge-text">{t('{kind} 有更新', { kind: botKind })}</span></>}
      </button>
      {confirmDialog}
    </>
  )
}

function ConfirmRestart({
  open,
  kind,
  notice,
  needsInstall,
  name,
  busy,
  asking,
  changelog,
  onCancel,
  onConfirm,
  onAskAgm,
}: {
  open: boolean
  kind: string
  notice: string
  needsInstall: boolean
  name: string
  busy: boolean
  asking: boolean
  changelog: ReactNode
  onCancel: () => void
  onConfirm: () => void
  onAskAgm?: () => void
}) {
  const t = useT()
  return (
    <ConfirmDialog
      open={open}
      title={needsInstall ? t('{kind} 有新版，還沒安裝', { kind }) : t('套用 {kind} 更新？', { kind })}
      body={
        <>
          {changelog}
          {needsInstall ? (
            <p>
              {t.x('{notice}。重啟 {name} 換不到新版：請先在終端跑 {kind} 的安裝指令（畫面上那句 {hint}，或選單的 {cmd}），裝好之後這裡會改成「重啟套用」。AG Man 不會自動安裝。', {
                notice,
                name: <strong>{name}</strong>,
                kind,
                hint: <code>Run … to update</code>,
                cmd: <code>Update now</code>,
              })}
            </p>
          ) : busy ? (
            <p>
              {t.x('{name} 正在忙，現在重啟會打斷這一回合。新版 {kind} 會 {resume} 接著同一個 session 跑，但這一輪沒說完的話會斷在那裡。', {
                name: <strong>{name}</strong>,
                kind,
                resume: <code>--resume</code>,
              })}
            </p>
          ) : (
            <p>
              {t.x('重啟 {name} 會用新版 {kind} 接著跑（session 會 {resume}，上下文不會掉）。', {
                name: <strong>{name}</strong>,
                kind,
                resume: <code>--resume</code>,
              })}
            </p>
          )}
        </>
      }
      confirmLabel={needsInstall ? t('知道了') : t('重啟套用')}
      secondaryLabel={onAskAgm ? (asking ? t('派工中…') : t('分析影響')) : undefined}
      secondaryDisabled={asking}
      onSecondary={onAskAgm}
      danger={busy}
      width={440}
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  )
}
