/**
 * 介面語言偏好與目前語言（issue #1932）。
 *
 * 偏好 `am-lang`：`zh-TW`／`en`／（沒存＝跟隨瀏覽器）。跟隨瀏覽器：`navigator.language(s)` 以 zh 開頭→繁中，其餘→英文。
 * 測試環境固定繁中：`test/node-test-shim.ts`（bunfig preload）設 `globalThis.__AM_SYSTEM_LANG__`，既有測試不受瀏覽器語言影響。
 * mock 另有網址參數 `?lang=en` 固定語言（`setLangOverride`，不寫入儲存），拍截圖用。
 */
import type { Lang, LangPref } from './types'

const KEY = 'am-lang'

declare global {
  // eslint-disable-next-line no-var
  var __AM_SYSTEM_LANG__: Lang | undefined
}

/** `navigator.languages`／`language` → 介面語言：任何一個 zh 開頭（含 zh-CN、zh-HK）→ 繁中，其餘 → 英文。只看第一順位。 */
export function detectSystemLang(languages: readonly string[] | string | null | undefined): Lang {
  const first = typeof languages === 'string' ? languages : languages?.[0]
  return typeof first === 'string' && /^zh\b/i.test(first) ? 'zh-TW' : 'en'
}

function systemLang(): Lang {
  if (globalThis.__AM_SYSTEM_LANG__) return globalThis.__AM_SYSTEM_LANG__
  if (typeof navigator === 'undefined') return 'zh-TW'
  return detectSystemLang(navigator.languages?.length ? navigator.languages : navigator.language)
}

export function parsePref(v: string | null | undefined): LangPref {
  return v === 'zh-TW' || v === 'en' ? v : 'system'
}

export function loadLangPref(): LangPref {
  try {
    return parsePref(localStorage.getItem(KEY))
  } catch {
    return 'system'
  }
}

function saveLangPref(p: LangPref): void {
  try {
    if (p === 'system') localStorage.removeItem(KEY)
    else localStorage.setItem(KEY, p)
  } catch {
    // 私密視窗存不了就只套這一次。
  }
}

interface State {
  pref: LangPref
  override: Lang | null
  lang: Lang
}
let state: State | null = null
const listeners = new Set<() => void>()

function resolve(pref: LangPref, override: Lang | null): Lang {
  return override ?? (pref === 'system' ? systemLang() : pref)
}

function paintHtmlLang(lang: Lang): void {
  if (typeof document !== 'undefined') document.documentElement.lang = lang === 'en' ? 'en' : 'zh-Hant'
}

function ensure(): State {
  if (!state) {
    const pref = loadLangPref()
    state = { pref, override: null, lang: resolve(pref, null) }
    paintHtmlLang(state.lang)
  }
  return state
}

function commit(next: Partial<State>): void {
  const cur = ensure()
  const pref = next.pref ?? cur.pref
  const override = next.override === undefined ? cur.override : next.override
  const lang = resolve(pref, override)
  const changed = lang !== cur.lang || pref !== cur.pref || override !== cur.override
  state = { pref, override, lang }
  paintHtmlLang(lang)
  if (changed) for (const fn of [...listeners]) fn()
}

export function getLang(): Lang {
  return ensure().lang
}
export function getLangPref(): LangPref {
  return ensure().pref
}

/** 設偏好並存起來（環境設定的切換）；使用者明確選了，網址固定的語言就讓位。有東西變了才通知訂閱者。 */
export function setLangPref(pref: LangPref): void {
  saveLangPref(pref)
  commit({ pref, override: null })
}

/** 暫時固定語言（網址 `?lang=`），優先於偏好、不寫入儲存；傳 null 取消。 */
export function setLangOverride(lang: Lang | null): void {
  commit({ override: lang })
}

export function subscribeLang(fn: () => void): () => void {
  listeners.add(fn)
  return () => {
    listeners.delete(fn)
  }
}

/** 網址 `?lang=en`／`?lang=zh`（`zh-TW`、`zh-Hant` 也認）→ 語言；沒給或不認得回 null。 */
export function langFromSearch(search: string): Lang | null {
  const v = new URLSearchParams(search).get('lang')
  if (!v) return null
  return /^en\b/i.test(v) ? 'en' : /^zh\b/i.test(v) ? 'zh-TW' : null
}

/**
 * 另一個分頁改了語言偏好（`storage` 事件只送給其他分頁）：跟著換，不寫回儲存。
 * 不是這個鍵回 false。
 */
export function langFromStorageEvent(key: string | null, newValue: string | null): boolean {
  if (key !== null && key !== KEY) return false
  commit({ pref: parsePref(newValue) })
  return true
}

/** 測試／重置用：丟掉快取的狀態，下一次讀取重新從儲存與瀏覽器算。 */
export function resetLangForTest(): void {
  state = null
  listeners.clear()
}
