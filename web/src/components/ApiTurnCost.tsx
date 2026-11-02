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

/** live bubble 的進度列：第 N 次請求 · 已花 $X（含預留 $Y）。 */
export function ApiTurnProgress({ botId }: { botId: string }) {
  const t = useT()
  const turnSig = useStore((s) => Object.values(s.turns[botId] ?? {}).find((x) => x.status === 'in_flight')?.id ?? null)
  const turns = useApiTurnCosts(botId, true)
  const turn = turnSig ? turns.find((x) => x.turn_id === turnSig) : undefined
  if (!turn) return null
  return (
    <span className="msg-cost mono">
      {t('第 {n} 次請求 · 已花 ${x}（含預留 ${y}）', { n: turn.requests, x: turn.cost_usd.toFixed(3), y: turn.reserved_usd.toFixed(3) })}
    </span>
  )
}
