/**
 * 訊息翻譯的共用狀態（使用者 2026-11-04：「點過翻譯不要自動換回原文，維持譯文優先，除非 user 再次 click」）。
 * 狀態放在元件外、以 msg.id 為鍵：訊息列重新掛載（捲動虛擬化、切 bot、訊息更新）時直接拿回上次那一面與譯文，不重打 API。
 * 「選了看譯文」的 msg id 另存 localStorage（最近 500 則），重整後掛載時由 hook 自動重抓（daemon 有檔案快取，命中不扣費）。
 * 模組層不呼叫 msg()／api／store：純資料，文案與 API 都在 hook 裡。
 */

export type TranslationEntry =
  | { status: 'idle' | 'loading' | 'error' }
  /** 品質檢查沒過（daemon 422 translate_rejected）：這則翻不好，不是服務壞掉；顯示原文＋小提示，手動鈕照常。 */
  | { status: 'rejected' }
  | { status: 'done'; text: string; show: boolean; cost: number | null; cached: boolean; model: string }

const IDLE: TranslationEntry = { status: 'idle' }
const KEY = 'am.msgTranslate.want'
/** 使用者手動點回原文的 msg id（自動翻譯不再翻回去；使用者 2026-11-06）。 */
const DECLINED_KEY = 'am.msgTranslate.declined'
export const MAX_REMEMBERED = 500
/** 自動翻譯連續失敗幾次後，這次頁面生命週期內停手（超過花費上限、API 掛了時不轟炸）。 */
export const AUTO_FAIL_LIMIT = 3

const entries = new Map<string, TranslationEntry>()
const listeners = new Set<() => void>()
/** 這次頁面生命週期已自動試過的 id：失敗不一直重試。 */
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

function loadDeclined(): string[] {
  if (declined) return declined
  let list: string[] = []
  try {
    const v: unknown = JSON.parse(localStorage.getItem(DECLINED_KEY) ?? '[]')
    if (Array.isArray(v)) list = v.filter((x): x is string => typeof x === 'string').slice(-MAX_REMEMBERED)
  } catch {
    /* localStorage 不可用或壞資料：當空 */
  }
  declined = list
  return list
}

/** 這則使用者手動點回過原文：自動翻譯不再翻它。 */
export function isDeclined(id: string): boolean {
  return loadDeclined().includes(id)
}

/** 記住／取消記住「這則使用者要看原文」；新加的放最後，超過上限丟最舊的。 */
export function setDeclined(id: string, on: boolean) {
  const cur = loadDeclined()
  if (on === cur.includes(id)) return
  const next = on ? [...cur, id].slice(-MAX_REMEMBERED) : cur.filter((x) => x !== id)
  declined = next
  try {
    localStorage.setItem(DECLINED_KEY, JSON.stringify(next))
  } catch {
    /* 不可用時只當次有效 */
  }
}

/** 自動翻譯「真的」失敗一次（網路、5xx、設定、預算；累計到上限就停手，成功一次就歸零）。品質檢查沒過不算，不要呼叫這個。 */
export function noteAutoFailure() {
  autoFailures += 1
}

/** 任何一次翻譯成功：連續失敗計數歸零（偶發失敗不該累積到整頁停手）。 */
export function noteTranslationSuccess() {
  autoFailures = 0
}

/** 第一次自動翻這則回 true（之後回 false）。 */
export function claimAutoTranslate(id: string): boolean {
  if (autoTried.has(id)) return false
  autoTried.add(id)
  return true
}

/** 自動翻譯（新訊息）是否還該動作：連續失敗太多次就不動，要翻請手動按。 */
export function autoTranslateAllowed(): boolean {
  return autoFailures < AUTO_FAIL_LIMIT
}

/** 給測試：清掉記憶體狀態（localStorage 由測試自己清）。 */
export function resetTranslationStore() {
  entries.clear()
  autoTried.clear()
  wanted = null
  declined = null
  autoFailures = 0
  emit()
}
