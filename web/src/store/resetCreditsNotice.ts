import type { QuotaMap } from '../api/types'
import { t } from '../lib/i18n'
import { CREDIT_NOTICE_MS, fmtCreditDate } from '../lib/resetCredits'
import { useStore } from './store'

const STORE_KEY = 'agm.resetCredits.v1'

export type CreditChange = { key: string; from: number; to: number }

export function resetCreditChanges(seen: Record<string, number>, quota: QuotaMap): { changes: CreditChange[]; next: Record<string, number> } {
  const next = { ...seen }
  const changes: CreditChange[] = []
  for (const [key, q] of Object.entries(quota)) {
    const kind = key.split('/').pop()!.split(':')[0]
    if (kind !== 'codex' || !q || q.stale || !q.reset_credits) continue
    const n = q.reset_credits.available
    if (key in seen && seen[key] !== n) changes.push({ key, from: seen[key], to: n })
    next[key] = n
  }
  return { changes, next }
}

export function creditKeyLabel(key: string): string {
  const slash = key.indexOf('/')
  const host = slash >= 0 ? key.slice(0, slash) : ''
  const [kind, identity] = key.slice(slash + 1).split(':')
  return `${kind}${identity ? ` · ${identity}` : ''}${host ? ` @${host}` : ''}`
}

function load(): Record<string, number> {
  try {
    const v = JSON.parse(localStorage.getItem(STORE_KEY) ?? '{}')
    return v && typeof v === 'object' ? v : {}
  } catch {
    return {}
  }
}

function save(v: Record<string, number>) {
  try {
    localStorage.setItem(STORE_KEY, JSON.stringify(v))
  } catch {
  }
}

export function startResetCreditsNotice(): () => void {
  let seen = load()
  let last: QuotaMap | null = null
  const check = (quota: QuotaMap) => {
    if (quota === last) return
    last = quota
    const { changes, next } = resetCreditChanges(seen, quota)
    if (JSON.stringify(next) !== JSON.stringify(seen)) {
      seen = next
      save(next)
    }
    for (const c of changes) {
      useStore.getState().notify('info', t('{name} 的重置券：{from} → {to} 張', { name: creditKeyLabel(c.key), from: c.from, to: c.to }))
    }
  }
  check(useStore.getState().quota)
  const unsub = useStore.subscribe((s) => check(s.quota))
  return () => {
    unsub()
  }
}

const EXPIRY_KEY = 'agm.resetCreditExpiry.v1'
const EXPIRY_KEEP_MS = 7 * 24 * 3600_000

export type CreditExpiry = { key: string; id: string; title: string | null; expiresAt: string }

export function resetCreditExpiries(seen: Record<string, number>, quota: QuotaMap, now: number): { due: CreditExpiry[]; next: Record<string, number> } {
  const next: Record<string, number> = {}
  for (const [id, at] of Object.entries(seen)) if (now - at < EXPIRY_KEEP_MS) next[id] = at
  const due: CreditExpiry[] = []
  for (const [key, q] of Object.entries(quota)) {
    const kind = key.split('/').pop()!.split(':')[0]
    if (kind !== 'codex' || !q || q.stale || !q.reset_credits) continue
    for (const c of q.reset_credits.credits) {
      if (!c.id || (c.status !== 'available' && c.status !== 'redeeming') || c.id in next) continue
      const at = c.expires_at ? Date.parse(c.expires_at) : NaN
      if (Number.isNaN(at) || at <= now || at - now > CREDIT_NOTICE_MS) continue
      next[c.id] = now
      due.push({ key, id: c.id, title: c.title, expiresAt: c.expires_at! })
    }
  }
  return { due, next }
}

export function creditExpiryText(d: CreditExpiry): string {
  return t('{name} 的重置券『{title}』{time} 到期（24 小時內），在 codex 用 /status → Reset usage', {
    name: creditKeyLabel(d.key),
    title: d.title ?? t('額度重置券'),
    time: fmtCreditDate(d.expiresAt),
  })
}

function loadExpiry(): Record<string, number> {
  try {
    const v = JSON.parse(localStorage.getItem(EXPIRY_KEY) ?? '{}')
    return v && typeof v === 'object' ? v : {}
  } catch {
    return {}
  }
}

function saveExpiry(v: Record<string, number>) {
  try {
    localStorage.setItem(EXPIRY_KEY, JSON.stringify(v))
  } catch {
  }
}

let expiryTimer: ReturnType<typeof setInterval> | null = null

export function startResetCreditExpiryNotice(now: () => number = Date.now): () => void {
  let seen = loadExpiry()
  const check = () => {
    const { due, next } = resetCreditExpiries(seen, useStore.getState().quota, now())
    if (due.length === 0 && Object.keys(next).length === Object.keys(seen).length) return
    seen = next
    saveExpiry(next)
    for (const d of due) useStore.getState().notify('info', creditExpiryText(d))
  }
  check()
  let last = useStore.getState().quota
  const unsub = useStore.subscribe((s) => {
    if (s.quota === last) return
    last = s.quota
    check()
  })
  if (expiryTimer) clearInterval(expiryTimer)
  expiryTimer = setInterval(check, 10 * 60_000)

  return () => {
    unsub()
    if (expiryTimer) {
      clearInterval(expiryTimer)
      expiryTimer = null
    }
  }
}

if (import.meta.hot) {
  import.meta.hot.dispose(() => {
    if (expiryTimer) {
      clearInterval(expiryTimer)
      expiryTimer = null
    }
  })
}
