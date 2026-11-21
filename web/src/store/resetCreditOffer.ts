import { useSyncExternalStore } from 'react'
import type { KindQuota, QuotaMap } from '../api/types'
import { t } from '../lib/i18n'
import { redeemResetCredit, snoozeResetCredit } from '../api'
import { fmtCreditDate } from '../lib/resetCredits'
import { useStore } from './store'
import { creditKeyLabel } from './resetCreditsNotice'

const OFFER_KEY = 'agm.resetCreditOffer.v1'
const OFFER_KEEP_MS = 7 * 24 * 3600_000

export interface ResetCreditOffer {
  id: string
  key: string
  host: string
  identity: string | null
  available: number
  title: string | null
  expiresAt: string | null
  heldBots: string[]
}

export function exhaustionMarker(q: KindQuota, now: number): string | null {
  const hit = q.limit_hit
  if (hit && (!hit.until || Date.parse(hit.until) > now)) return `hit:${hit.at}`
  for (const [name, w] of [['five_hour', q.five_hour], ['seven_day', q.seven_day]] as const) {
    if (!w || w.used_pct < 100 || !w.resets_at) continue
    if (Date.parse(w.resets_at) > now) return `win:${name}:${w.resets_at}`
  }
  return null
}

export function parseCreditKey(key: string): { host: string; identity: string | null } {
  const slash = key.indexOf('/')
  const host = slash >= 0 ? key.slice(0, slash) : 'local'
  const colon = key.indexOf(':', slash + 1)
  const identity = colon >= 0 ? key.slice(colon + 1) : null
  return { host, identity: identity || null }
}

export function resetCreditOffers(
  seen: Record<string, number>,
  quota: QuotaMap,
  now: number,
): { offers: ResetCreditOffer[]; next: Record<string, number> } {
  const next: Record<string, number> = {}
  for (const [id, at] of Object.entries(seen)) if (now - at < OFFER_KEEP_MS) next[id] = at
  const offers: ResetCreditOffer[] = []
  for (const [key, q] of Object.entries(quota)) {
    const kind = key.split('/').pop()!.split(':')[0]
    if (kind !== 'codex' || !q || q.stale || !q.reset_credits || q.reset_credits.available <= 0) continue
    if (snoozedUntilMs(q, now) !== null) continue
    const marker = exhaustionMarker(q, now)
    if (!marker) continue
    const id = `${key}|${marker}`
    if (id in next) continue
    next[id] = now
    const { host, identity } = parseCreditKey(key)
    const rc = q.reset_credits
    offers.push({
      id,
      key,
      host,
      identity,
      available: rc.available,
      title: rc.title,
      expiresAt: rc.expires_at,
      heldBots: q.held_bots ?? [],
    })
  }
  return { offers, next }
}

export function snoozedUntilMs(q: KindQuota, now: number): number | null {
  const until = q.reset_offer_snoozed_until ? Date.parse(q.reset_offer_snoozed_until) : NaN
  return Number.isFinite(until) && until > now ? until : null
}

export function resetCreditOfferText(o: ResetCreditOffer): string {
  const soonest = o.expiresAt ? t('（最早 {time} 到期）', { time: fmtCreditDate(o.expiresAt) }) : ''
  const held = o.heldBots.length > 0 ? t('停工的 bot：{names}。', { names: o.heldBots.join('、') }) : ''
  return t('{name} 額度用完了。{held}還有 {n} 張重置券{soonest}，要用一張嗎？', {
    name: creditKeyLabel(o.key),
    held,
    n: o.available,
    soonest,
  })
}

function loadSeen(): Record<string, number> {
  try {
    const v = JSON.parse(localStorage.getItem(OFFER_KEY) ?? '{}')
    return v && typeof v === 'object' ? v : {}
  } catch {
    return {}
  }
}

let seenState: Record<string, number> | null = null
function currentSeen(): Record<string, number> {
  if (seenState === null) seenState = loadSeen()
  return seenState
}

function saveSeen(v: Record<string, number>) {
  try {
    localStorage.setItem(OFFER_KEY, JSON.stringify(v))
  } catch {
  }
}

let queue: ResetCreditOffer[] = []
const listeners = new Set<() => void>()
function emit() {
  for (const l of listeners) l()
}
function subscribe(l: () => void) {
  listeners.add(l)
  return () => {
    listeners.delete(l)
  }
}

export function useResetCreditOffer(): ResetCreditOffer | null {
  return useSyncExternalStore(subscribe, () => queue[0] ?? null)
}

export function startResetCreditOfferNotice(now: () => number = Date.now): () => void {
  seenState = loadSeen()
  let last: QuotaMap | null = null
  const check = (quota: QuotaMap) => {
    if (quota === last) return
    last = quota
    const seen = currentSeen()
    const { offers, next } = resetCreditOffers(seen, quota, now())
    if (JSON.stringify(next) !== JSON.stringify(seen)) {
      seenState = next
      saveSeen(next)
    }
    if (offers.length === 0) return
    queue = [...queue, ...offers]
    emit()
  }
  check(useStore.getState().quota)
  const unsub = useStore.subscribe((s) => check(s.quota))
  return () => {
    unsub()
  }
}

export async function snoozeResetCreditOffer(o: ResetCreditOffer, hours: 1 | 24 | 72): Promise<boolean> {
  const notify = useStore.getState().notify
  let until: string
  try {
    until = (await snoozeResetCredit({ key: o.key, hours })).snoozedUntil
  } catch (e) {
    notify('error', t('暫停提醒失敗：{msg}', { msg: e instanceof Error ? e.message : String(e) }))
    return false
  }
  const seen = { ...currentSeen() }
  delete seen[o.id]
  seenState = seen
  saveSeen(seen)
  useStore.setState((s) => {
    const cur = s.quota[o.key]
    return cur ? { quota: { ...s.quota, [o.key]: { ...cur, reset_offer_snoozed_until: until } } } : {}
  })
  queue = queue.filter((x) => x.id !== o.id)
  emit()
  notify('info', t('{name} 的重置券提示已暫停到 {time}。', { name: creditKeyLabel(o.key), time: fmtCreditDate(until) }))
  return true
}

export async function answerResetCreditOffer(o: ResetCreditOffer, use: boolean): Promise<boolean> {
  queue = queue.filter((x) => x.id !== o.id)
  emit()
  if (!use) return true
  const notify = useStore.getState().notify
  try {
    const r = await redeemResetCredit({ host: o.host, identity: o.identity })
    if (r.outcome === 'reset') {
      notify('info', t('{name} 已使用一張重置券，額度重置了。', { name: creditKeyLabel(o.key) }))
      if (r.resumed.length > 0) notify('info', t('已通知 {n} 顆停工的 bot 繼續工作。', { n: r.resumed.length }))
    } else if (r.outcome === 'alreadyRedeemed') {
      notify('info', t('這張重置券剛才已經用過了。'))
    } else {
      notify('info', t('這次沒有需要重置的額度（{outcome}）。', { outcome: r.outcome }))
    }
    return true
  } catch (e) {
    notify('error', t('使用重置券失敗：{msg}', { msg: e instanceof Error ? e.message : String(e) }))
    return false
  }
}

export function reopenResetCreditOffer(key: string, now: () => number = Date.now): void {
  const tryOpen = (quota: QuotaMap): boolean => {
    const q = quota[key]
    if (!q) return false
    const { offers } = resetCreditOffers({}, { [key]: q }, now())
    if (offers.length === 0) return true
    queue = [...offers, ...queue.filter((x) => x.key !== key)]
    emit()
    return true
  }
  if (tryOpen(useStore.getState().quota)) return
  const unsub = useStore.subscribe((s) => {
    if (tryOpen(s.quota)) unsub()
  })
  setTimeout(unsub, 30_000)
}
