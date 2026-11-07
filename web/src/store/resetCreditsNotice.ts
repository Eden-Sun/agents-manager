import type { QuotaMap } from '../api/types'
import { t } from '../lib/i18n'
import { CREDIT_NOTICE_MS, fmtCreditDate } from '../lib/resetCredits'
import { useStore } from './store'

/**
 * codex 重置券（`reset_credits.available`）數量一變就跳通知（2026-11-01 使用者）。
 * 每個額度 key（`codex`、`codex:co0`、`m4p/codex:co0`）記住上次看到的張數（localStorage，重開頁面照樣比得出來）；
 * 第一次看到只當基準不通知；讀不到（null／stale）不算數，避免換方案暫缺欄或舊讀數誤報。
 */
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

/** `m4p/codex:co0` → `codex · co0 @m4p`。 */
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
    // 私密視窗／封鎖儲存：只是下次重開不比對，不影響畫面。
  }
}

export function startResetCreditsNotice() {
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
  useStore.subscribe((s) => check(s.quota))
}

/**
 * 到期前一天提示（2026-11-07 使用者）：每張券（用 id 去重）在到期前 24 小時內提示一次。
 * 不依賴網頁開著的那一份在 daemon（AGM inbox `ops_alert`，SPEC §16）；這裡是網頁開著時的 toast。
 * 去重記在 localStorage（`id → 提示時間`），超過 7 天的記錄清掉。
 */
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
    // 同上：存不了只是重開後可能再提示一次。
  }
}

/** 先記再跳：存不了就不跳，免得每一輪都重複打擾；單一 toast 一張券。 */
export function startResetCreditExpiryNotice(now: () => number = Date.now) {
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
  useStore.subscribe((s) => {
    if (s.quota === last) return
    last = s.quota
    check()
  })
  // 頁面一直開著時，券會從 24 小時外走進 24 小時內，額度沒變也要重看。
  setInterval(check, 10 * 60_000)
}
