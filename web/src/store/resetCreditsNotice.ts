import type { QuotaMap } from '../api/types'
import { t } from '../lib/i18n'
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
