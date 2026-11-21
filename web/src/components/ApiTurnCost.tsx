import { isApiKind } from './kindMeta'
import { useEffect, useState } from 'react'
import type { ApiTurnCost } from '../api/apiBotUsage'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { useApiTurnCosts } from '../hooks/useApiTurnCosts'

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
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind)
  const isApi = isApiKind(kind)
  const turns = useApiTurnCosts(isApi ? botId : undefined)
  const turn = turnId ? turns.find((x) => x.turn_id === turnId) : undefined
  if (!turn || turn.in_flight) return null
  if (kind === 'openai-api') return <OpenAiTurnCost turn={turn} onToggle={onToggle} expanded={expanded} />
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

function OpenAiTurnCost({ turn, onToggle, expanded }: { turn: ApiTurnCost; onToggle?: () => void; expanded?: boolean }) {
  const t = useT()
  const breakdown = t('輸入 {i} · 快取讀 {r} · 快取寫 {w} · 輸出 {o}', {
    i: turn.input_tokens, r: turn.cache_read_tokens, w: turn.cache_write_tokens, o: turn.output_tokens,
  })
  const reasoning = turn.reasoning_tokens ? t(' · reasoning {n}（已含在輸出）', { n: turn.reasoning_tokens }) : ''
  const cost = `$${turn.cost_usd.toFixed(3)}`
  const detail = (
    <span className="msg-cost mono api-openai-breakdown" data-k="openai-breakdown" title={t('{n} 次請求', { n: turn.requests })}>
      {breakdown}{reasoning}
    </span>
  )
  if (onToggle) {
    return (
      <>
        <button
          type="button"
          className={`msg-cost mono api-turn-cost-btn${expanded ? ' expanded' : ''}`}
          onClick={onToggle}
          aria-expanded={expanded}
          aria-label={t('花費 {cost}，展開查看時間軸', { cost })}
        >
          <span>{cost}</span>
          <span className="api-cost-arrow" aria-hidden="true">{expanded ? ' ▾' : ' ▸'}</span>
        </button>
        {detail}
      </>
    )
  }
  return (
    <>
      <span className="msg-cost mono">{cost}</span>
      {detail}
    </>
  )
}

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
  const openai = useStore((s) => s.bots.find((b) => b.id === botId)?.kind === 'openai-api')
  if (est !== null && !openai) {
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
