import { useSyncExternalStore } from 'react'
import type { KindQuota, QuotaMap } from '../api/types'
import { t } from '../lib/i18n'
import { redeemResetCredit } from '../api'
import { fmtCreditDate } from '../lib/resetCredits'
import { useStore } from './store'
import { creditKeyLabel } from './resetCreditsNotice'

/**
 * 額度用完、帳號還有重置券時，問使用者要不要用一張（2026-11-08 使用者）。
 *
 * 「用完」＝撞限橫幅（`limit_hit`，還沒過期）或 5 小時／週窗已到 100%（且重置時間還在後面）。
 * 每次耗盡事件（同一個帳號、同一個撞限紀錄或窗口重置時刻）只問一次：一跳出來就記下，不論按「使用一張」或「不用」，
 * 這次耗盡都不再問；下一次耗盡是新的事件才會再問。同帳號的多顆 bot 共用同一個額度 key，自然合併成一則。
 * 去重記在 localStorage（`agm.resetCreditOffer.v1`，超過 7 天的清掉）；存不了只是重開後可能再問一次。
 * 使用券由 daemon 的 User-only 端點執行（`POST /api/quota/reset-credit/use`），網頁不直接花券。
 */
const OFFER_KEY = 'agm.resetCreditOffer.v1'
const OFFER_KEEP_MS = 7 * 24 * 3600_000

export interface ResetCreditOffer {
  /** 耗盡事件的去重 id（key＋事件標記）。 */
  id: string
  key: string
  host: string
  identity: string | null
  available: number
  title: string | null
  expiresAt: string | null
  /** 因為這個額度停工的 bot 名稱（daemon 的 `held_bots`）。 */
  heldBots: string[]
}

/** 這個額度格現在是不是用完了；是就回傳事件標記（同一次耗盡標記不變）。 */
export function exhaustionMarker(q: KindQuota, now: number): string | null {
  const hit = q.limit_hit
  if (hit && (!hit.until || Date.parse(hit.until) > now)) return `hit:${hit.at}`
  for (const [name, w] of [['five_hour', q.five_hour], ['seven_day', q.seven_day]] as const) {
    if (!w || w.used_pct < 100 || !w.resets_at) continue
    if (Date.parse(w.resets_at) > now) return `win:${name}:${w.resets_at}`
  }
  return null
}

/** `m4p/codex:co0` → host `m4p`、identity `co0`；本機 `codex` → host `local`、identity null。 */
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

function saveSeen(v: Record<string, number>) {
  try {
    localStorage.setItem(OFFER_KEY, JSON.stringify(v))
  } catch {
    // 存不了：只是重開後可能再問一次。
  }
}

// 待問的隊列（一次只顯示第一張）。獨立的小 store，不改全域 store 的形狀。
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

/** 跟額度資料一起看：新的耗盡事件進隊列；已問過的不再問。 */
export function startResetCreditOfferNotice(now: () => number = Date.now): () => void {
  let seen = loadSeen()
  let last: QuotaMap | null = null
  const check = (quota: QuotaMap) => {
    if (quota === last) return
    last = quota
    const { offers, next } = resetCreditOffers(seen, quota, now())
    if (JSON.stringify(next) !== JSON.stringify(seen)) {
      seen = next
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

/** 用掉當前這張（或只是關掉它）。回傳 `true` 表示 daemon 回成功。 */
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

/** 點推播通知：把這個帳號目前的耗盡提示再開一次（不看「問過了」的紀錄）。額度資料還沒到就等它到。 */
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
