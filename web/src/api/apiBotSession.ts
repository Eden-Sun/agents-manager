/** claude-api bot 的對話管理（#1944）：查目前 session（`GET /api/bots/{id}/api_fresh_target`）與退役（清掉對話）。 */
import { rawTransport } from './index'
import { isRec, num, optStr, pick } from './normalize'

export interface ApiFreshTarget {
  availability: 'available' | 'none' | 'unavailable'
  session_id: string | null
  generation: number | null
}

export async function apiFreshTarget(botId: string): Promise<ApiFreshTarget> {
  const raw = await rawTransport.request('GET', `/bots/${encodeURIComponent(botId)}/api_fresh_target`)
  const o = isRec(raw) ? raw : {}
  const a = optStr(pick(o, 'availability'))
  return {
    availability: a === 'available' || a === 'none' ? a : 'unavailable',
    session_id: optStr(pick(o, 'session_id')),
    generation: typeof pick(o, 'generation') === 'number' ? num(pick(o, 'generation'), 0) : null,
  }
}

/** 清掉目前的 API 對話；下一則訊息從新對話開始。沒有可清的 session 回 false。 */
export async function retireApiSession(botId: string): Promise<boolean> {
  const target = await apiFreshTarget(botId)
  if (target.availability !== 'available' || !target.session_id || !target.generation) return false
  await rawTransport.request('POST', `/bots/${encodeURIComponent(botId)}/api-session/retire`, { session_id: target.session_id, generation: target.generation })
  return true
}
