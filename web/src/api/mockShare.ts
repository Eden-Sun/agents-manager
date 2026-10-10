/**
 * mock 的分享管理端點（API.md §5.6）：`GET/POST /bots/{id}/share`、`POST /bots/{id}/share/rotate`。
 * 規則照契約：只有 `share_profile = restricted` 的 bot 能開（其他 409 `not_shareable`）；開著就回完整 url（daemon 存 token 原文）。
 * `allow_embed` 只有信任分享能設（受限帶了 400 `share_embed_trusted_only`），預設 false。
 */
import { ApiError } from './types'

type Rec = Record<string, unknown>

interface Share {
  token: string
  created_at: string
  last_used_at: string | null
  allow_embed: boolean
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

  private changed: (botId: string, enabled: boolean) => void

  private trusted: (botId: string) => boolean

  constructor(restricted: (botId: string) => boolean | null, changed: (botId: string, enabled: boolean) => void = () => {}, trusted: (botId: string) => boolean = () => false) {
    this.restricted = restricted
    this.changed = changed
    this.trusted = trusted
  }

  isEnabled(botId: string): boolean {
    return this.shares.has(botId)
  }

  /** 種子資料：已經開著的分享。 */
  seed(botId: string) {
    this.shares.set(botId, { token: newToken(), created_at: new Date(Date.now() - 86_400_000).toISOString(), last_used_at: new Date(Date.now() - 600_000).toISOString(), allow_embed: false })
  }

  private view(botId: string): Rec {
    const s = this.shares.get(botId)
    if (!s) return { enabled: false, url: null, needs_rotate: false, token_hint: null, created_at: null, last_used_at: null, allow_embed: false }
    return {
      enabled: true,
      url: `${MOCK_SHARE_BASE}/s/${s.token}`,
      needs_rotate: false,
      token_hint: `…${s.token.slice(-4)}`,
      created_at: s.created_at,
      last_used_at: s.last_used_at,
      allow_embed: s.allow_embed,
    }
  }

  handle(method: string, path: string, b: Rec): unknown {
    const m = path.match(/^\/bots\/([^/]+)\/share(\/rotate)?$/)
    if (!m) return undefined
    const botId = decodeURIComponent(m[1])
    const r = this.restricted(botId)
    if (r === null) throw new ApiError(404, { error: 'not_found', what: 'bot' }, 'not found')
    if (!r) throw new ApiError(409, { error: 'conflict', reason: 'not_shareable' }, 'not shareable')
    if (method === 'GET' && !m[2]) return this.view(botId)
    if (method === 'POST' && m[2]) {
      if (!this.shares.has(botId)) throw new ApiError(409, { error: 'conflict', reason: 'share_disabled' }, 'share disabled')
      this.shares.set(botId, { token: newToken(), created_at: new Date().toISOString(), last_used_at: null, allow_embed: false })
      this.changed(botId, true)
      return this.view(botId)
    }
    if (method === 'POST') {
      if (b.allow_embed !== undefined && !this.trusted(botId)) throw new ApiError(400, { error: 'bad_request', reason: 'share_embed_trusted_only' }, 'share embed trusted only')
      if (b.enabled === true) {
        // 已開著：不換 token，回同一條。
        if (!this.shares.has(botId)) {
          this.shares.set(botId, { token: newToken(), created_at: new Date().toISOString(), last_used_at: null, allow_embed: false })
          this.changed(botId, true)
        }
        if (b.allow_embed !== undefined) this.shares.get(botId)!.allow_embed = b.allow_embed === true
        return this.view(botId)
      }
      if (this.shares.delete(botId)) this.changed(botId, false)
      return this.view(botId)
    }
    return undefined
  }
}
