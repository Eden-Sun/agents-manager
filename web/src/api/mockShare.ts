/**
 * mock 的分享管理端點（API.md §5.6）：`GET/POST /bots/{id}/share`、`POST /bots/{id}/share/rotate`。
 * 規則照契約：只有 `share_profile = restricted` 的 bot 能開（其他 409 `not_shareable`）；完整 url 只在開／重產那一次回。
 */
import { ApiError } from './types'

type Rec = Record<string, unknown>

interface Share {
  token: string
  created_at: string
  last_used_at: string | null
}

/** mock 的分享入口網址（真 daemon 讀 config `[share] base_url`）。 */
export const MOCK_SHARE_BASE = 'https://agm.tail161aae.ts.net'

function newToken(): string {
  const bytes = new Uint8Array(32)
  crypto.getRandomValues(bytes)
  let s = ''
  for (const b of bytes) s += String.fromCharCode(b)
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

export class MockShares {
  private shares = new Map<string, Share>()
  private restricted: (botId: string) => boolean | null

  constructor(restricted: (botId: string) => boolean | null) {
    this.restricted = restricted
  }

  /** 種子資料：已經開著的分享（GET 只看得到末 4 碼）。 */
  seed(botId: string) {
    this.shares.set(botId, { token: newToken(), created_at: new Date(Date.now() - 86_400_000).toISOString(), last_used_at: new Date(Date.now() - 600_000).toISOString() })
  }

  private view(botId: string, withUrl: boolean): Rec {
    const s = this.shares.get(botId)
    if (!s) return { enabled: false, url: null, token_hint: null, created_at: null, last_used_at: null }
    return {
      enabled: true,
      url: withUrl ? `${MOCK_SHARE_BASE}/s/${s.token}` : null,
      token_hint: `…${s.token.slice(-4)}`,
      created_at: s.created_at,
      last_used_at: s.last_used_at,
    }
  }

  handle(method: string, path: string, b: Rec): unknown {
    const m = path.match(/^\/bots\/([^/]+)\/share(\/rotate)?$/)
    if (!m) return undefined
    const botId = decodeURIComponent(m[1])
    const r = this.restricted(botId)
    if (r === null) throw new ApiError(404, { error: 'not_found', what: 'bot' }, 'not found')
    if (!r) throw new ApiError(409, { error: 'conflict', reason: 'not_shareable' }, 'not shareable')
    if (method === 'GET' && !m[2]) return this.view(botId, false)
    if (method === 'POST' && m[2]) {
      if (!this.shares.has(botId)) throw new ApiError(409, { error: 'conflict', reason: 'share_disabled' }, 'share disabled')
      this.shares.set(botId, { token: newToken(), created_at: new Date().toISOString(), last_used_at: null })
      return this.view(botId, true)
    }
    if (method === 'POST') {
      if (b.enabled === true) {
        // 已開著：不換 token，也拿不到完整連結（daemon 只存雜湊）。
        if (this.shares.has(botId)) return this.view(botId, false)
        this.shares.set(botId, { token: newToken(), created_at: new Date().toISOString(), last_used_at: null })
        return this.view(botId, true)
      }
      this.shares.delete(botId)
      return this.view(botId, false)
    }
    return undefined
  }
}
