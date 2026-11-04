/**
 * 訊息翻譯的共用狀態（使用者 2026-11-04：「點過翻譯不要自動換回原文，維持譯文優先，除非 user 再次 click」）。
 * 狀態放在元件外、以 msg.id 為鍵：訊息列重新掛載（捲動虛擬化、切 bot、訊息更新）時直接拿回上次那一面與譯文，不重打 API。
 * 「選了看譯文」的 msg id 另存 localStorage（最近 500 則），重整後掛載時由 hook 自動重抓（daemon 有檔案快取，命中不扣費）。
 * 模組層不呼叫 msg()／api／store：純資料，文案與 API 都在 hook 裡。
 */

export type TranslationEntry =
  | { status: 'idle' | 'loading' | 'error' }
  | { status: 'done'; text: string; show: boolean; cost: number | null; cached: boolean; model: string }

const IDLE: TranslationEntry = { status: 'idle' }
const KEY = 'am.msgTranslate.want'
export const MAX_REMEMBERED = 500

const entries = new Map<string, TranslationEntry>()
const listeners = new Set<() => void>()
/** 這次頁面生命週期已自動試過的 id：失敗不一直重試。 */
const autoTried = new Set<string>()
let wanted: string[] | null = null

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
    /* localStorage 不可用或壞資料：當空 */
  }
  wanted = list
  return list
}

function saveWanted(list: string[]) {
  wanted = list
  try {
    localStorage.setItem(KEY, JSON.stringify(list))
  } catch {
    /* 不可用時只當次有效 */
  }
}

/** 這則記著要看譯文（重整後自動翻）。 */
export function isWanted(id: string): boolean {
  return loadWanted().includes(id)
}

/** 記住／取消記住「這則看譯文」；新加的放最後，超過上限丟最舊的。 */
export function setWanted(id: string, on: boolean) {
  const cur = loadWanted()
  const has = cur.includes(id)
  if (on === has && (!on || cur[cur.length - 1] === id)) return
  const rest = cur.filter((x) => x !== id)
  saveWanted(on ? [...rest, id].slice(-MAX_REMEMBERED) : rest)
}

/** 第一次自動翻這則回 true（之後回 false）。 */
export function claimAutoTranslate(id: string): boolean {
  if (autoTried.has(id)) return false
  autoTried.add(id)
  return true
}

/** 給測試：清掉記憶體狀態（localStorage 由測試自己清）。 */
export function resetTranslationStore() {
  entries.clear()
  autoTried.clear()
  wanted = null
  emit()
}
