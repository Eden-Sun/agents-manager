import { create } from 'zustand'

export interface CodexResetInfo {
  source: string
  checked_at: string
  last_reset_at: string | null
  upcoming: boolean
  latest: { id: string; text: string; url: string; at: string } | null
  notice: { id: string; text: string; url: string; at: string } | null
}
const SOURCE = 'https://codex-reset.com/'
const SEEN = 'am:codex-reset-notices'
const delivered = new Set<string>()
const record = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null
function news(v: unknown): CodexResetInfo['latest'] {
  if (!record(v) || typeof v.id !== 'string' || typeof v.text !== 'string' || typeof v.at !== 'string') return null
  return { id: v.id, text: v.text, at: v.at, url: SOURCE }
}
export const useCodexReset = create<{ info: CodexResetInfo | null }>(() => ({ info: null }))

/** Shared ledger across quota cells, reconnects and browser reloads. */
export function receiveCodexReset(raw: unknown, notify: (kind: 'info', text: string, action?: { label: string; run: () => void }) => void) {
  if (raw === null) { useCodexReset.setState({ info: null }); return }
  if (!record(raw) || typeof raw.checked_at !== 'string' || !Number.isFinite(Date.parse(raw.checked_at))) return
  const previous = useCodexReset.getState().info
  if (previous && Date.parse(previous.checked_at) > Date.parse(raw.checked_at)) return
  const info: CodexResetInfo = {
    source: SOURCE, checked_at: raw.checked_at, upcoming: raw.upcoming === true,
    last_reset_at: typeof raw.last_reset_at === 'string' && Number.isFinite(Date.parse(raw.last_reset_at)) ? raw.last_reset_at : null,
    latest: news(raw.latest), notice: news(raw.notice),
  }
  useCodexReset.setState({ info })
  if (!info.notice || !Number.isFinite(Date.parse(info.notice.at)) || Date.now() - Date.parse(info.checked_at) > 70 * 60_000 || Date.now() - Date.parse(info.notice.at) > 48 * 60 * 60_000) return
  let seen: string[] = []
  try { const parsed: unknown = JSON.parse(localStorage.getItem(SEEN) ?? '[]'); if (Array.isArray(parsed)) seen = parsed.filter((x): x is string => typeof x === 'string') } catch { /* Storage can be unavailable. */ }
  if (seen.includes(info.notice.id) || delivered.has(info.notice.id)) return
  delivered.add(info.notice.id)
  if (delivered.size > 100) delivered.delete(delivered.values().next().value!)
  try { localStorage.setItem(SEEN, JSON.stringify([...seen.slice(-99), info.notice.id])) } catch { /* In-memory dedupe still applies. */ }
  notify('info', `Codex 額度新消息：${info.notice.text}（資料：codex-reset.com）`, {
    label: '查看來源', run: () => { window.open(SOURCE, '_blank', 'noopener,noreferrer') },
  })
}
