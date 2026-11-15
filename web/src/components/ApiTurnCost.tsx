/** claude-api bot 每則回覆的單價與進行中的進度。 */
import { useEffect, useState } from 'react'
import type { ApiTurnCost } from '../api/apiBotUsage'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { useApiTurnCosts } from '../hooks/useApiTurnCosts'

/** 每則 assistant 回覆時間旁的單價。可展開看時間軸與拆分。 */
export function ApiTurnCostTag({
  botId,
  turnId,
  expanded,
  onToggle,
}: {
  botId: string
  turnId: string | null
  expanded?: boolean
  onToggle?: () => void
}) {
  const t = useT()
  const isApi = useStore((s) => s.bots.find((b) => b.id === botId)?.kind === 'claude-api')
  const turns = useApiTurnCosts(isApi ? botId : undefined)
  const turn = turnId ? turns.find((x) => x.turn_id === turnId) : undefined
  if (!turn || turn.in_flight) return null
  const tip = t('{n} 次請求 · 輸入 {i} · 輸出 {o} · 快取讀 {r} · 快取寫 {w}', {
    n: turn.requests, i: turn.input_tokens, o: turn.output_tokens, r: turn.cache_read_tokens, w: turn.cache_write_tokens,
  })
  if (onToggle) {
    return (
      <button
        type="button"
        className={`msg-cost mono api-turn-cost-btn${expanded ? ' expanded' : ''}`}
        onClick={onToggle}
        title={t('{tip}（點擊展開時間軸）', { tip })}
        aria-expanded={expanded}
        aria-label={t('花費 {cost}，展開查看時間軸', { cost: `$${turn.cost_usd.toFixed(3)}` })}
      >
        <span>${turn.cost_usd.toFixed(3)}</span>
        <span className="api-cost-arrow" aria-hidden="true">{expanded ? ' ▾' : ' ▸'}</span>
      </button>
    )
  }
  return <span className="msg-cost mono" title={tip}>${turn.cost_usd.toFixed(3)}</span>
}

/**
 * live bubble 的進度列：第 N 次請求 · 已花 $X（已結算）＋預留 $Y（進行中請求的上限，不算進 X）。
 * 串流中（`turn_progress.est_cost_microusd`）改顯示「≈ $X 目前為止」：已結算的請求＋這一次請求到目前為止的估價，取代預留；
 * 估價只是畫面用的，結算後回到帳上的真實數字。
 */
/**
 * 進行中回合的時鐘：已進行多久／時限、第幾次／呼叫上限。回合可能一口氣跑好幾分鐘、十幾次呼叫都沒有文字輸出，
 * 沒有這行使用者只看到「等待回覆」，分不出是在跑還是被吞掉（2026-11-15 pt-hub）。到軟期限（時限的 85%）模型會先收尾寫摘要。
 */
function ApiTurnClock({ turn }: { turn: ApiTurnCost }) {
  const t = useT()
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [])
  const started = turn.started_at ? Date.parse(turn.started_at) : NaN
  if (!Number.isFinite(started)) return null
  const secs = Math.max(0, Math.floor((now - started) / 1000))
  const mmss = (s: number) => `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`
  const limit = turn.turn_timeout_secs
  return (
    <span
      className="msg-cost mono"
      data-k="api-turn-clock"
      title={t('這一回合最多 {calls} 次呼叫、{limit} 秒；時間到會先請模型收尾寫摘要，不是直接中止', { calls: turn.max_calls ?? '?', limit: limit ?? '?' })}
    >
      {t('已進行 {t}', { t: mmss(secs) })}
      {limit ? ` / ${mmss(limit)}` : ''}
      {turn.max_calls ? t('（第 {n}/{m} 次呼叫）', { n: turn.requests, m: turn.max_calls }) : ''}
    </span>
  )
}

export function ApiTurnProgress({ botId }: { botId: string }) {
  const t = useT()
  const turnSig = useStore((s) => Object.values(s.turns[botId] ?? {}).find((x) => x.status === 'in_flight')?.id ?? null)
  const est = useStore((s) => {
    const live = s.liveReply[botId]
    return live && live.turnId === turnSig ? live.estCostMicrousd : null
  })
  const turns = useApiTurnCosts(botId, true)
  const turn = turnSig ? turns.find((x) => x.turn_id === turnSig) : undefined
  // 速度每回合開始時讀一次設定、整回合固定：設定在回合中途改了，這一回合仍是舊速度，下一回合才換。
  const configured = useStore((s) => {
    const b = s.bots.find((x) => x.id === botId)
    return b ? (b.fast ? 'fast' : 'standard') : null
  })
  const speedNote =
    turn?.speed && configured && turn.speed !== configured ? (
      <span className="msg-cost mono api-speed-note" role="status">
        {t('本回合仍是 {cur}，下一回合起改為 {next}', { cur: turn.speed, next: configured })}
      </span>
    ) : null
  if (est !== null) {
    const x = (turn?.cost_usd ?? 0) + est / 1_000_000
    return (
      <>
        <span className="msg-cost mono">{t('第 {n} 次請求 · ≈ ${x} 目前為止', { n: turn?.requests ?? 1, x: x.toFixed(3) })}</span>
        {speedNote}
      </>
    )
  }
  if (!turn) return null
  return (
    <>
      <span className="msg-cost mono">
        {t('第 {n} 次請求 · 已花 ${x}', { n: turn.requests, x: turn.cost_usd.toFixed(3) })}
        {turn.reserved_usd > 0 ? <span className="api-usage-hold" data-k="turn-reserved"> {t('＋預留 ${x}', { x: turn.reserved_usd.toFixed(3) })}</span> : null}
      </span>
      <ApiTurnClock turn={turn} />
      {speedNote}
    </>
  )
}
