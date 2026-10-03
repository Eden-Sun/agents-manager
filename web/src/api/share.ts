/**
 * 分享 bot 的管理端點（主 API、只收 UI token；SPEC「分享 bot」、API.md §5.6）：
 * `GET/POST /api/bots/{id}/share`、`POST /api/bots/{id}/share/rotate`。
 *
 * 完整連結只在剛開／重產那一次回來（daemon 只存 token 的 hash），平常只有 `token_hint`（末 4 碼）。
 */
import { rawTransport } from './index'
import { ApiError } from './types'

export interface ShareState {
  enabled: boolean
  /** 完整連結；只有剛開／重產的回應才有，之後的 GET 一律 null。 */
  url: string | null
  /** `…末4碼`，讓人認得現在是哪一條連結。 */
  token_hint: string | null
  created_at: string | null
  last_used_at: string | null
}

const optStr = (v: unknown): string | null => (typeof v === 'string' && v ? v : null)

export function toShareState(raw: unknown): ShareState {
  const o = typeof raw === 'object' && raw !== null ? (raw as Record<string, unknown>) : {}
  return {
    enabled: o.enabled === true,
    url: optStr(o.url),
    token_hint: optStr(o.token_hint),
    created_at: optStr(o.created_at),
    last_used_at: optStr(o.last_used_at),
  }
}

const path = (botId: string) => `/bots/${encodeURIComponent(botId)}/share`

export async function fetchShare(botId: string): Promise<ShareState> {
  return toShareState(await rawTransport.request('GET', path(botId)))
}

export async function setShareEnabled(botId: string, enabled: boolean): Promise<ShareState> {
  return toShareState(await rawTransport.request('POST', path(botId), { enabled }))
}

export async function rotateShare(botId: string): Promise<ShareState> {
  return toShareState(await rawTransport.request('POST', `${path(botId)}/rotate`))
}

/** daemon 的 409 reason 換成人話；其他錯誤照原文。 */
export function shareErrorText(e: unknown): string {
  if (e instanceof ApiError) {
    const b = e.body as Record<string, unknown> | undefined
    const reason = typeof b?.reason === 'string' ? b.reason : typeof b?.error === 'string' ? b.error : ''
    if (reason === 'share_not_configured') return '還沒設定分享入口：config.toml 的 [share] base_url（Tailscale Funnel 的網址）沒填'
    if (reason === 'not_shareable') return '只有「分享用（受限）」的 bot 能開分享連結'
    if (reason === 'unsupported_kind') return '分享用的受限 bot 目前只支援 claude'
    const msg = typeof b?.message === 'string' ? b.message : ''
    return msg || reason || e.message
  }
  return e instanceof Error ? e.message : String(e)
}
