import { useEffect } from 'react'
import { createPortal } from 'react-dom'
import { effortLabel } from '../api/types'
import { useBotLamp } from '../hooks/useBotLamp'
import { backgroundTaskLines } from '../lib/backgroundJobs'
import { shortModel } from '../lib/shortModel'
import { liveReplyOf, useStore } from '../store/store'
import { cleanLiveActivity } from '../store/liveText'
import { StatusLamp } from './StatusLamp'
import './chipLegend.css'

/** 晶片列給的這顆 bot 的提示狀態（跟晶片顏色同一份判斷，免得卡片跟顏色講不一樣）。 */
export interface ChipHints {
  current: boolean
  unread: number
  needsReply: boolean
  waitsKids: boolean
  kidsRunning: number
  /** 快取倒數的說明（`cacheClock`）；沒有是 null。 */
  cacheTitle: string | null
}

/** 先講「該不該去看」，跟晶片的顏色同一個優先序：要你回答 → 未讀 → 等子 agent → 在跑。 */
function headline(h: ChipHints, label: string, blockedReason: string): string {
  if (h.needsReply) return blockedReason ? `停在等你回答：${blockedReason}` : '停在等你回答'
  if (h.unread > 0) return `有 ${h.unread} 個回合做完了還沒看`
  if (h.waitsKids) return '在等子 agent（或子 agent 回報了還沒人看）'
  return label
}

/**
 * 主力 bot 現在的狀態（2026-10-04 使用者：「hover 至主力的 bot 時，就說明目前 context 狀態」）。
 * 電腦：滑鼠停在主力晶片上；手機：長按不動放開。`anchor` 有值＝貼在那顆晶片下方的浮卡，沒有＝手機底部彈出。
 */
export function BotStatusCard({
  botId,
  hints,
  anchor,
  onClose,
  onLegend,
}: {
  botId: string
  hints: ChipHints
  anchor: DOMRect | null
  onClose: () => void
  onLegend: () => void
}) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const run = useStore((s) => s.runs[botId] ?? null)
  const activity = useStore((s) => cleanLiveActivity(liveReplyOf(s, botId)?.activity))
  const { lamp, background, label, blockedReason } = useBotLamp(botId)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])
  if (!bot) return null

  const status = run?.status ?? null
  const title = run?.agent_title?.trim() || ''
  const doing = activity || title
  const tasks = backgroundTaskLines(run)
  const model = shortModel(bot.kind, run?.runtime_model ?? bot.model ?? status?.model_name ?? null)
  const effort = status?.effort ?? run?.runtime_effort ?? bot.effort
  const identity = run?.runtime_identity ?? bot.identity
  const ctx =
    status?.context_used_pct != null
      ? `${Math.round(status.context_used_pct)}%${
          status.context_used_tokens != null && status.context_size != null
            ? `（${Math.round(status.context_used_tokens / 1000)}k / ${Math.round(status.context_size / 1000)}k）`
            : ''
        }`
      : null

  const card = (
    <div className="bot-status-card chip-legend" role="dialog" aria-label={`${bot.name} 的狀態`} onClick={(e) => e.stopPropagation()}>
      <div className="chip-legend-head">
        <span className="bot-status-name">
          <StatusLamp lamp={lamp} background={background} />
          <strong>{bot.name}</strong>
        </span>
        {anchor ? null : (
          <button type="button" className="chip-legend-close" aria-label="關閉" onClick={onClose}>
            ✕
          </button>
        )}
      </div>
      <p className="bot-status-headline">{headline(hints, label, blockedReason)}</p>
      <dl className="bot-status-rows">
        {doing ? (
          <>
            <dt>正在做</dt>
            <dd>{doing}</dd>
          </>
        ) : null}
        {background > 0 ? (
          <>
            <dt>背景</dt>
            <dd>{tasks.length ? tasks.join(' · ') : `${background} 個工作還在跑`}</dd>
          </>
        ) : null}
        {hints.kidsRunning > 0 ? (
          <>
            <dt>子 agent</dt>
            <dd>{hints.kidsRunning} 個在跑</dd>
          </>
        ) : null}
        {ctx ? (
          <>
            <dt>context</dt>
            <dd>已用 {ctx}</dd>
          </>
        ) : null}
        {model ? (
          <>
            <dt>模型</dt>
            <dd>
              {model}
              {effort ? ` · ${effortLabel(effort)}` : ''}
              {identity ? ` · ${identity}` : ''}
            </dd>
          </>
        ) : null}
        {hints.cacheTitle ? (
          <>
            <dt>快取</dt>
            <dd>{hints.cacheTitle}</dd>
          </>
        ) : null}
      </dl>
      <button type="button" className="bot-status-legend" onClick={onLegend}>
        顏色代表什麼？
      </button>
    </div>
  )

  if (anchor) {
    // 浮卡：貼在晶片下方、不蓋暗底；超出右緣就往左收。
    const left = Math.max(8, Math.min(anchor.left, window.innerWidth - 336))
    return createPortal(
      <div className="bot-status-float" style={{ top: anchor.bottom + 6, left }}>
        {card}
      </div>,
      document.body,
    )
  }
  return createPortal(
    <div className="chip-legend-backdrop" onClick={onClose}>
      {card}
    </div>,
    document.body,
  )
}
