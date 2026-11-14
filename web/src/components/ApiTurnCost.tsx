/** claude-api bot 每則回覆的單價與進行中的進度。 */
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { useApiTurnCosts } from '../hooks/useApiTurnCosts'

/** 每則 assistant 回覆時間旁的單價。 */
export function ApiTurnCostTag({ botId, turnId }: { botId: string; turnId: string | null }) {
  const t = useT()
  const isApi = useStore((s) => s.bots.find((b) => b.id === botId)?.kind === 'claude-api')
  const turns = useApiTurnCosts(isApi ? botId : undefined)
  const turn = turnId ? turns.find((x) => x.turn_id === turnId) : undefined
  if (!turn || turn.in_flight) return null
  const tip = t('{n} 次請求 · 輸入 {i} · 輸出 {o} · 快取讀 {r} · 快取寫 {w}', {
    n: turn.requests, i: turn.input_tokens, o: turn.output_tokens, r: turn.cache_read_tokens, w: turn.cache_write_tokens,
  })
  return <span className="msg-cost mono" title={tip}>${turn.cost_usd.toFixed(3)}</span>
}

/**
 * live bubble 的進度列：第 N 次請求 · 已花 $X（含預留 $Y）；同 ApiBotUsageBar，預留先當花掉算進 X。
 * 串流中（`turn_progress.est_cost_microusd`）改顯示「≈ $X 目前為止」：已結算的請求＋這一次請求到目前為止的估價，取代預留；
 * 估價只是畫面用的，結算後回到帳上的真實數字。
 */
export function ApiTurnProgress({ botId }: { botId: string }) {
  const t = useT()
  const turnSig = useStore((s) => Object.values(s.turns[botId] ?? {}).find((x) => x.status === 'in_flight')?.id ?? null)
  const est = useStore((s) => {
    const live = s.liveReply[botId]
    return live && live.turnId === turnSig ? live.estCostMicrousd : null
  })
  const turns = useApiTurnCosts(botId, true)
  const turn = turnSig ? turns.find((x) => x.turn_id === turnSig) : undefined
  if (est !== null) {
    const x = (turn?.cost_usd ?? 0) + est / 1_000_000
    return <span className="msg-cost mono">{t('第 {n} 次請求 · ≈ ${x} 目前為止', { n: turn?.requests ?? 1, x: x.toFixed(3) })}</span>
  }
  if (!turn) return null
  return (
    <span className="msg-cost mono">
      {t('第 {n} 次請求 · 已花 ${x}（含預留 ${y}）', { n: turn.requests, x: (turn.cost_usd + turn.reserved_usd).toFixed(3), y: turn.reserved_usd.toFixed(3) })}
    </span>
  )
}
