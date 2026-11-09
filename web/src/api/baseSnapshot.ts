/** codex bot 的基準快照（`/api/bots/{id}/base-snapshot`）：鎖定目前 context、清除、查詢。 */
import { rawTransport } from './index'
import { isRec, optStr, pick } from './normalize'

export interface BaseSnapshot {
  locked: boolean
  session_id: string | null
  locked_at: string | null
}

const url = (botId: string) => `/bots/${encodeURIComponent(botId)}/base-snapshot`

function parse(raw: unknown): BaseSnapshot {
  const o = isRec(raw) ? raw : {}
  const locked = pick(o, 'locked') === true
  return { locked, session_id: locked ? optStr(pick(o, 'session_id')) : null, locked_at: locked ? optStr(pick(o, 'locked_at')) : null }
}

export const getBaseSnapshot = async (botId: string): Promise<BaseSnapshot> => parse(await rawTransport.request('GET', url(botId)))
export const lockBaseSnapshot = async (botId: string): Promise<BaseSnapshot> => parse(await rawTransport.request('PUT', url(botId), {}))
export const clearBaseSnapshot = async (botId: string): Promise<BaseSnapshot> => parse(await rawTransport.request('DELETE', url(botId)))
