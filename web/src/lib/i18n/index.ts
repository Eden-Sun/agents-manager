import { createElement, Fragment, useMemo, useSyncExternalStore, type ReactNode } from 'react'
import { EN } from '../../i18n/en'
import { getLang, subscribeLang } from './lang'
import { translate, translateParts } from './translate'
import type { Lang, LiteralKey, TFn, Vars } from './types'

export type { Dict, DictEntry, Lang, LangPref, PluralEntry, TFn, Vars } from './types'
export { getLang, getLangPref, langFromSearch, langFromStorageEvent, resetLangForTest, setLangOverride, setLangPref, subscribeLang } from './lang'

const missing = new Set<string>()
const onMissing = (key: string) => missing.add(key)

type NodeVars = Readonly<Record<string, ReactNode>>
const nodes = (lang: Lang, key: string, vars: NodeVars): ReactNode =>
  createElement(Fragment, null, ...translateParts(lang, EN, key, vars, onMissing).map((p, i) => createElement(Fragment, { key: i }, p)))

function make(lang: Lang) {
  const tr = (key: string, vars?: Vars) => translate(lang, EN, key, vars, onMissing)
  const x = <K extends string>(key: LiteralKey<K>, vars: NodeVars) => nodes(lang, key, vars)
  return Object.assign(<K extends string>(key: LiteralKey<K>, vars?: Vars) => tr(key, vars), { tr, x })
}

export const t: TFn = <K extends string>(key: LiteralKey<K>, vars?: Vars) => translate(getLang(), EN, key, vars, onMissing)
export const tr = (key: string, vars?: Vars): string => translate(getLang(), EN, key, vars, onMissing)
export function msg<K extends string>(key: LiteralKey<K>): K { return key as unknown as K }

export function useT() {
  const lang = useLang()
  return useMemo(() => make(lang), [lang])
}

export function useLang(): Lang {
  return useSyncExternalStore(subscribeLang, getLang, getLang)
}

export function intlLocale(lang: Lang = getLang()): string {
  return lang === 'en' ? 'en-US' : 'zh-TW'
}

export function fmtNumber(n: number, opts?: Intl.NumberFormatOptions): string {
  return n.toLocaleString(intlLocale(), opts)
}

export function fmtDate(date: Date | number, opts: Intl.DateTimeFormatOptions = { dateStyle: 'medium', timeStyle: 'short' }): string {
  return new Intl.DateTimeFormat(intlLocale(), opts).format(date)
}

export function missingKeys(): string[] {
  return [...missing]
}
