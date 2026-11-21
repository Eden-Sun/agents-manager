
export interface RejectDetail {
  check: string
  src_latin: number
  src_cjk: number
  reply_latin: number
  reply_cjk: number
  identical: boolean
  models: string[]
  attempts: number
}

export type TranslationEntry =
  | { status: 'idle' | 'loading' | 'error' }
  | { status: 'rejected'; reason?: string; text?: string; detail?: RejectDetail }
  | {
      status: 'done'
      text: string
      show: boolean
      ready?: boolean
      cost: number | null
      cached: boolean
      model: string
      lowConfidence?: boolean
      jevProb?: number | null
    }

const IDLE: TranslationEntry = { status: 'idle' }
const KEY = 'am.msgTranslate.want'
const DECLINED_KEY = 'am.msgTranslate.declined'
export const MAX_REMEMBERED = 500
export const AUTO_FAIL_LIMIT = 3

const entries = new Map<string, TranslationEntry>()
const listeners = new Set<() => void>()
const autoTried = new Set<string>()
let wanted: string[] | null = null
let declined: string[] | null = null
let autoFailures = 0

function emit() {
  for (const l of listeners) l()
}

export function subscribeTranslations(fn: () => void): () => void {
  listeners.add(fn)
  return () => {
    listeners.delete(fn)
  }
}

export function getTranslation(id: string): TranslationEntry {
  return entries.get(id) ?? IDLE
}

export function setTranslation(id: string, e: TranslationEntry) {
  entries.set(id, e)
  emit()
}

function loadWanted(): string[] {
  if (wanted) return wanted
  let list: string[] = []
  try {
    const v: unknown = JSON.parse(localStorage.getItem(KEY) ?? '[]')
    if (Array.isArray(v)) list = v.filter((x): x is string => typeof x === 'string').slice(-MAX_REMEMBERED)
  } catch {
  }
  wanted = list
  return list
}

function saveWanted(list: string[]) {
  wanted = list
  try {
    localStorage.setItem(KEY, JSON.stringify(list))
  } catch {
  }
}

export function isWanted(id: string): boolean {
  return loadWanted().includes(id)
}

export function setWanted(id: string, on: boolean) {
  const cur = loadWanted()
  const has = cur.includes(id)
  if (on === has && (!on || cur[cur.length - 1] === id)) return
  const rest = cur.filter((x) => x !== id)
  saveWanted(on ? [...rest, id].slice(-MAX_REMEMBERED) : rest)
}

function loadDeclined(): string[] {
  if (declined) return declined
  let list: string[] = []
  try {
    const v: unknown = JSON.parse(localStorage.getItem(DECLINED_KEY) ?? '[]')
    if (Array.isArray(v)) list = v.filter((x): x is string => typeof x === 'string').slice(-MAX_REMEMBERED)
  } catch {
  }
  declined = list
  return list
}

export function isDeclined(id: string): boolean {
  return loadDeclined().includes(id)
}

export function setDeclined(id: string, on: boolean) {
  const cur = loadDeclined()
  if (on === cur.includes(id)) return
  const next = on ? [...cur, id].slice(-MAX_REMEMBERED) : cur.filter((x) => x !== id)
  declined = next
  try {
    localStorage.setItem(DECLINED_KEY, JSON.stringify(next))
  } catch {
  }
}

export function noteAutoFailure() {
  autoFailures += 1
}

export function noteTranslationSuccess() {
  autoFailures = 0
}

export function claimAutoTranslate(id: string): boolean {
  if (autoTried.has(id)) return false
  autoTried.add(id)
  return true
}

export function autoTranslateAllowed(): boolean {
  return autoFailures < AUTO_FAIL_LIMIT
}

export function resetTranslationStore() {
  entries.clear()
  autoTried.clear()
  wanted = null
  declined = null
  autoFailures = 0
  emit()
}
