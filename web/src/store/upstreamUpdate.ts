import { writeShared } from './mobilePreview'
import { t } from '../lib/i18n'

const KEY = 'am.upstreamUpdate.seen'

export interface UpstreamItem {
  kind: string
  latest: string | null
  target: string | null
  hasUpdate: boolean
  hosts: { host: string; installedVersion: string | null; error: string | null; behind: boolean }[]
  notify: 'update' | 'error' | null
  text: string | null
  autoUpdate?: boolean
}

function isRec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null
}

export function parseUpstreamItem(raw: unknown): UpstreamItem | null {
  if (!isRec(raw) || typeof raw.kind !== 'string') return null
  const notify = raw.notify === 'update' || raw.notify === 'error' ? raw.notify : null
  return {
    kind: raw.kind,
    latest: typeof raw.latest_version === 'string' ? raw.latest_version : null,
    target: typeof raw.target_version === 'string' ? raw.target_version : null,
    hasUpdate: raw.has_update === true,
    hosts: Array.isArray(raw.hosts)
      ? raw.hosts.flatMap((entry) => {
          if (!isRec(entry) || typeof entry.host !== 'string') return []
          return [{
            host: entry.host,
            installedVersion: typeof entry.installed_version === 'string' ? entry.installed_version : null,
            error: typeof entry.error === 'string' ? entry.error : null,
            behind: entry.behind === true,
          }]
        })
      : [],
    notify,
    text: typeof raw.text === 'string' && raw.text ? raw.text : null,
    autoUpdate: raw.auto_update === true,
  }
}

function versionParts(v: string): number[] | null {
  const tok = (v.trim().split(/\s+/)[0] ?? '').replace(/^\[|\]$/g, '').replace(/^v/, '')
  const parts = tok.split('.').map((p) => (/^\d+$/.test(p) ? Number(p) : NaN))
  return parts.every(Number.isFinite) ? parts : null
}

export function isNewerVersion(latest: string, seen: string | undefined): boolean {
  if (seen === undefined) return true
  const a = versionParts(latest)
  const b = versionParts(seen)
  if (!a || !b) return latest !== seen
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const x = a[i] ?? 0
    const y = b[i] ?? 0
    if (x !== y) return x > y
  }
  return false
}

export interface UpstreamNotice {
  kind: 'info' | 'error'
  text: string
  detail: string
}

export function upstreamToastText(item: UpstreamItem): string {
  if (item.notify === 'error') return t('查 {kind} 新版失敗', { kind: item.kind })
  return t('{kind} {version} 可更新', { kind: item.kind, version: item.latest ?? '' })
}

export function upstreamNotice(item: UpstreamItem, seen: Record<string, string>): UpstreamNotice | null {
  if (!item.text) return null
  if (item.notify === 'error') return { kind: 'error', text: upstreamToastText(item), detail: item.text }
  if (item.hasUpdate && item.latest && isNewerVersion(item.latest, seen[item.kind])) return { kind: 'info', text: upstreamToastText(item), detail: item.text }
  return null
}

function readSeen(): Record<string, string> {
  try {
    const v: unknown = JSON.parse(localStorage.getItem(KEY) ?? '{}')
    return isRec(v) ? (Object.fromEntries(Object.entries(v).filter(([, x]) => typeof x === 'string')) as Record<string, string>) : {}
  } catch {
    return {}
  }
}

function markSeen(kind: string, version: string) {
  writeShared(() => {
    try {
      localStorage.setItem(KEY, JSON.stringify({ ...readSeen(), [kind]: version }))
    } catch {
    }
  })
}

type Notify = (kind: 'info' | 'error', text: string, action?: { label: string; run: () => void }) => void

export function applyUpstreamItem(raw: unknown, notify: Notify, openChip: (kind: string) => void = () => {}): UpstreamItem | null {
  const item = parseUpstreamItem(raw)
  if (!item) return null
  const n = upstreamNotice(item, readSeen())
  if (!n) return item
  if (n.kind === 'info' && item.latest) markSeen(item.kind, item.latest)
  const action = n.kind === 'info'
    ? { label: t('查看'), run: () => openChip(item.kind) }
    : { label: t('詳情'), run: () => notify('error', n.detail) }
  notify(n.kind, n.text, action)
  return item
}

export async function loadUpstreamUpdates(
  fetchItems: () => Promise<unknown>,
  notify: Notify,
  onItem?: (item: UpstreamItem) => void,
  openChip?: (kind: string) => void,
) {
  try {
    const r = await fetchItems()
    const items = isRec(r) && Array.isArray(r.items) ? r.items : []
    for (const raw of items) {
      if (!isRec(raw)) continue
      const item = applyUpstreamItem({ ...raw, notify: null }, notify, openChip)
      if (item) onItem?.(item)
    }
  } catch {
  }
}
