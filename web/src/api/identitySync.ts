import { BOT_KINDS, type BotKind, type IdentitySyncItem, type IdentitySyncPlan, type IdentitySyncResponse, type IdentitySyncResultItem } from './types'

type Rec = Record<string, unknown>
const isRec = (v: unknown): v is Rec => typeof v === 'object' && v !== null && !Array.isArray(v)
const str = (v: unknown): string => (typeof v === 'string' ? v : '')
const optStr = (v: unknown): string | null => (typeof v === 'string' && v !== '' ? v : null)
const arr = (v: unknown): unknown[] => (Array.isArray(v) ? v : [])

function kindOf(v: unknown): BotKind {
  return BOT_KINDS.find((k) => k === v) ?? 'claude'
}

function envOf(v: unknown): Record<string, string> {
  if (!isRec(v)) return {}
  return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, str(x)]))
}

function sourceOf(v: unknown): 'shell' | 'config' {
  return v === 'shell' ? 'shell' : 'config'
}

function parseItem(v: unknown): IdentitySyncItem | null {
  if (!isRec(v)) return null
  const name = str(v.name)
  if (!name) return null
  return {
    name,
    kind: kindOf(v.kind),
    source: sourceOf(v.source),
    status: v.status === 'different' ? 'different' : 'missing',
    can_apply: v.can_apply === true,
    local_env: envOf(v.local_env),
    remote_env: isRec(v.remote_env) ? envOf(v.remote_env) : null,
    alias_line: optStr(v.alias_line),
    note: optStr(v.note),
  }
}

export function toIdentitySyncPlan(raw: unknown, host: string): IdentitySyncPlan {
  const o = isRec(raw) ? raw : {}
  return {
    host: str(o.host) || host,
    remote_shell: optStr(o.remote_shell),
    remote_rc: optStr(o.remote_rc),
    already_synced: typeof o.already_synced === 'number' ? o.already_synced : 0,
    items: arr(o.items).flatMap((i) => {
      const item = parseItem(i)
      return item ? [item] : []
    }),
  }
}

function parseResult(v: unknown): IdentitySyncResultItem | null {
  if (!isRec(v)) return null
  const name = str(v.name)
  if (!name) return null
  const status = v.status === 'created' || v.status === 'would_create' || v.status === 'failed' ? v.status : 'skipped'
  return {
    name,
    kind: kindOf(v.kind),
    source: sourceOf(v.source),
    status,
    reason: optStr(v.reason),
    message: optStr(v.message),
    alias_line: optStr(v.alias_line),
    created_dirs: arr(v.created_dirs).map(str),
  }
}

export function toIdentitySyncResponse(raw: unknown, host: string): IdentitySyncResponse {
  const o = isRec(raw) ? raw : {}
  return {
    host: str(o.host) || host,
    dry_run: o.dry_run === true,
    backup_path: optStr(o.backup_path),
    results: arr(o.results).flatMap((r) => {
      const item = parseResult(r)
      return item ? [item] : []
    }),
  }
}
