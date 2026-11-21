import { rawTransport } from './index'
import { ApiError } from './types'
import { t } from '../lib/i18n'

export interface ShareState {
  enabled: boolean
  url: string | null
  needs_rotate: boolean
  token_hint: string | null
  created_at: string | null
  last_used_at: string | null
  allow_embed: boolean
}

const optStr = (v: unknown): string | null => (typeof v === 'string' && v ? v : null)

export function toShareState(raw: unknown): ShareState {
  const o = typeof raw === 'object' && raw !== null ? (raw as Record<string, unknown>) : {}
  return {
    enabled: o.enabled === true,
    url: optStr(o.url),
    needs_rotate: o.needs_rotate === true,
    token_hint: optStr(o.token_hint),
    created_at: optStr(o.created_at),
    last_used_at: optStr(o.last_used_at),
    allow_embed: o.allow_embed === true,
  }
}

const path = (botId: string) => `/bots/${encodeURIComponent(botId)}/share`

export async function fetchShare(botId: string): Promise<ShareState> {
  return toShareState(await rawTransport.request('GET', path(botId)))
}

export async function setShareEnabled(botId: string, enabled: boolean, allowEmbed?: boolean): Promise<ShareState> {
  const body = allowEmbed === undefined ? { enabled } : { enabled, allow_embed: allowEmbed }
  return toShareState(await rawTransport.request('POST', path(botId), body))
}

export async function rotateShare(botId: string): Promise<ShareState> {
  return toShareState(await rawTransport.request('POST', `${path(botId)}/rotate`))
}

export function shareErrorText(e: unknown): string {
  if (e instanceof ApiError) {
    const b = e.body as Record<string, unknown> | undefined
    const reason = typeof b?.reason === 'string' ? b.reason : typeof b?.error === 'string' ? b.error : ''
    if (reason === 'share_not_configured') return t('還沒設定分享入口：config.toml 的 [share] base_url（Tailscale Funnel 的網址）沒填')
    if (reason === 'not_shareable') return t('只有「分享用（受限）」的 bot 能開分享連結')
    if (reason === 'share_embed_trusted_only') return t('「允許 iframe 嵌入」只給信任分享的 bot')
    if (reason === 'unsupported_kind') return t('分享用的受限 bot 目前只支援 claude')
    const msg = typeof b?.message === 'string' ? b.message : ''
    return msg || reason || e.message
  }
  return e instanceof Error ? e.message : String(e)
}
