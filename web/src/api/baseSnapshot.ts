import { rawTransport } from './index'
import { isRec, optStr, pick } from './normalize'

export interface BaseBuild {
  state: 'building' | 'failed'
  message: string | null
}

export interface BaseSnapshot {
  locked: boolean
  session_id: string | null
  locked_at: string | null
  /** 背景製作的狀態：製作中，或最近一次失敗（附訊息）；沒有就是 null／undefined。 */
  build?: BaseBuild | null
}

const url = (botId: string) => `/bots/${encodeURIComponent(botId)}/base-snapshot`

function parseBuild(raw: unknown): BaseBuild | null {
  if (!isRec(raw)) return null
  const state = pick(raw, 'state')
  if (state !== 'building' && state !== 'failed') return null
  return { state, message: optStr(pick(raw, 'message')) ?? null }
}

function parse(raw: unknown): BaseSnapshot {
  const o = isRec(raw) ? raw : {}
  const locked = pick(o, 'locked') === true
  return { locked, session_id: locked ? optStr(pick(o, 'session_id')) : null, locked_at: locked ? optStr(pick(o, 'locked_at')) : null, build: parseBuild(pick(o, 'build')) }
}

export const getBaseSnapshot = async (botId: string): Promise<BaseSnapshot> => parse(await rawTransport.request('GET', url(botId)))
export const lockBaseSnapshot = async (botId: string): Promise<BaseSnapshot> => parse(await rawTransport.request('PUT', url(botId), {}))
export const clearBaseSnapshot = async (botId: string): Promise<BaseSnapshot> => parse(await rawTransport.request('DELETE', url(botId)))
/** 失敗後重新在背景製作（已有基準就不動）；立即回應，進度看 `base_snapshot_progress`。 */
export const buildBaseSnapshot = async (botId: string): Promise<BaseSnapshot> => parse(await rawTransport.request('POST', `${url(botId)}/build`, {}))
