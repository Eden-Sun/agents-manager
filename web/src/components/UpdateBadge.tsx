import { useState, type ReactNode } from 'react'
import * as api from '../api'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { reviewErrText } from '../lib/claudeReviewErr'
import { useT } from '../lib/i18n'
import { updateRange } from '../lib/updateRange'
import { isChildBot, passiveUpdateHint, updateBatchCounts } from '../lib/updateBatch'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import { AgmReviewBox } from './AgmReviewBox'
import './updateBadge.css'

/**
 * claude 下載好新版只在 pane 底部印 `Update installed`；daemon 讀成 `runs.update_notice`，這顆 chip 點下去走重啟套用。
 * 2026-09-10：一律先開確認框並列 changelog。`variant="dot"`：側欄 bot 列上的小 ⌃⌃（2026-09-11 使用者：「click to update latest」）。
 * 2026-09-11 使用者：chip 版在批次（`UpdateQuotaChip`）蓋得到時不畫，省標題列第一排寬度；判斷只用 `updateBatch.ts` 與 daemon 同一份規則。
 * 2026-09-19：「蓋得到」含在忙那組——額度列 chip 一樣畫著它。
 */
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
  // 這顆 bot 在別處（⟳ 徽章、設定面板）已經在重啟：store 的 busy 旗標，所有重啟鈕共用。
  const restartingShared = useStore((s) => Boolean(s.busy[`restart:${botId}`]))
  const restarting = restartingLocal || restartingShared
  const [confirming, setConfirming] = useState(false)
  const [asking, setAsking] = useState(false)
  const [reviewKey, setReviewKey] = useState(0)

  // selector 回純布林：`updateBatchCounts` 每次回新陣列，連 `useShallow` 都擋不住（見 `UpdateQuotaChip.tsx`）。
  // 忙碌的也算：額度列的 chip 把它列在「在忙」那組，照樣畫著；再畫一顆就是同一個 ⌃⌃ 兩次（2026-09-19 使用者：「logo 重工了」）。
  // 這顆 bot 自己要套用走 context bar 版本號旁的「升級」（`inline`）。
  const coveredByBatch = useStore((s) => {
    const { ready, busy, passive } = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
    return [ready, busy, passive].some((list) => list.some((b) => b.botId === botId))
  })
  // 子 agent／default session 不在一鍵重啟裡（header 不算它）：新版資訊留在這一列的 tooltip（#1911）。
  const passiveHint = useStore((s) => {
    const bot = s.bots.find((b) => b.id === botId)
    return bot ? passiveUpdateHint(bot, s.runs[botId]) : null
  })

  // 子 agent 由父 Bot 重開，`/restart` 會回 409：不給按鈕，只留被動提示（#1911）。
  const isChild = useStore((s) => {
    const bot = s.bots.find((b) => b.id === botId)
    return bot ? isChildBot(bot) : false
  })
  const notice = run?.update_notice ?? null
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
    // chip：context bar 的 inline 已畫同一則提示，標題列不再重複（跟批次蓋得到時不畫同一條線）。
    return null
  }

  const busy = run?.agent_status === 'working' || run?.agent_status === 'blocked'
  // 要看的版本區間：codex 的新版還沒裝、磁碟是舊的，兩個版本都從通知讀（issue #561）。
  const range = updateRange(botKind, notice, runningVersion)
  // 「請 AGM 解析」claude 與 codex 同一套（issue #561）；其他 kind 沒有 changelog 來源也沒有任務檔。
  const canReview = botKind === 'claude' || botKind === 'codex'
  // codex 的新版通常還沒安裝（issue #388）：重啟換不到任何東西，先講清楚要裝；裝好之後（通知寫「已安裝」）才是重啟套用。
  const needsInstall = botKind !== 'claude' && !notice.includes('已安裝') // i18n-ignore：比對 daemon 通知原文
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

  // 使用者不想現在重啟，但想知道「這版有沒有我們用得上的東西」：交給 AGM 解析，結論回更新框。
  // 唯讀，所以按完就關框，不擋重啟那條路（使用者 2026-09-19）。
  const askAgm = () => {
    setAsking(true)
    void api
      .requestUpdateReview({ kind: botKind, host, from: range.from, to: range.to })
      .then((r) => {
        // 不關框：結論就顯示在框裡（使用者 2026-09-19）。派完先刷新一次，之後打開也會再讀。
        setReviewKey((n) => n + 1)
        notify(
          'info',
          // 已經派過（自己按過，或 30 分鐘那支排程先派了）不是錯誤，照實說一次就好。
          r.duplicate
            ? t('{kind} {version} 已經派給 {bot} 解析過了，結論會回到這裡', { kind: botKind, version: r.version, bot: r.target_bot_name || 'AGM' })
            : t('已請 {bot} 解析 {kind} {version} 的 changelog，結論會回到這裡', { kind: botKind, version: r.version, bot: r.target_bot_name }),
        )
      })
      .catch((e: unknown) => notify('error', t('派不出去：{reason}', { reason: reviewErrText(e) })))
      .finally(() => setAsking(false))
  }

  const confirmDialog = (
    <ConfirmRestart
      open={confirming}
      kind={botKind}
      notice={notice}
      needsInstall={needsInstall}
      name={botName}
      busy={busy}
      asking={asking}
      changelog={
        confirming ? (
          <div className="update-split">
            <UpdateChangelog kind={botKind} host={host} from={range.from} to={range.to} />
            {canReview ? <AgmReviewBox kind={botKind} host={host} from={range.from} to={range.to} refreshKey={reviewKey} /> : null}
          </div>
        ) : null
      }
      onCancel={() => setConfirming(false)}
      onConfirm={restart}
      onAskAgm={canReview ? askAgm : undefined}
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

  // 批次蓋得到就不畫（見檔頭）；但確認框開著或重啟中要留著，否則對話框會被抽走。
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
      secondaryLabel={onAskAgm ? (asking ? t('派工中…') : t('請 AGM 解析')) : undefined}
      secondaryDisabled={asking}
      onSecondary={onAskAgm}
      danger={busy}
      width={440}
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  )
}
