import type { Lang, LangPref } from './types'

const KEY = 'am-lang'

declare global {
  // eslint-disable-next-line no-var
  var __AM_SYSTEM_LANG__: Lang | undefined
}

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
  if (typeof document !== 'undefined' && document.documentElement) document.documentElement.lang = lang === 'en' ? 'en' : 'zh-Hant'
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

export function setLangPref(pref: LangPref): void {
  saveLangPref(pref)
  commit({ pref, override: null })
}

export function setLangOverride(lang: Lang | null): void {
  commit({ override: lang })
}

export function subscribeLang(fn: () => void): () => void {
  listeners.add(fn)
  return () => {
    listeners.delete(fn)
  }
}

export function langFromSearch(search: string): Lang | null {
  const v = new URLSearchParams(search).get('lang')
  if (!v) return null
  return /^en\b/i.test(v) ? 'en' : /^zh\b/i.test(v) ? 'zh-TW' : null
}

export function langFromStorageEvent(key: string | null, newValue: string | null): boolean {
  if (key !== null && key !== KEY) return false
  commit({ pref: parsePref(newValue) })
  return true
}

export function resetLangForTest(): void {
  state = null
  listeners.clear()
}
