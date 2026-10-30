/**
 * 網頁 UI 多語系（issue #1932；規則見 docs/FRONTEND.md〈多語系〉）。
 *
 * 繁中原句就是 key：`t('已連線')`、`t('{n} 個 Bot', { n })`；英文放 `src/i18n/en/<區塊>.ts`，缺了退回繁中。
 * - 元件：`const t = useT()`（切語言會重繪）。
 * - 非元件（lib、store 的 notify 文字）：直接 `import { t }`，呼叫當下的語言；不要在模組載入時就呼叫 `t()` 存起來。
 * - 標籤表／依狀態挑文字：定義處 `msg('未安裝')`（原樣回傳，只是讓掃描抓得到 key），顯示處 `t.tr(label)`／`tr(label)`。
 */
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
/** 句子裡塞元件：`t.x('已連線到 {host}', { host: <code>m4p</code> })`。`vars` 的值可以是任何 React 節點。 */
const nodes = (lang: Lang, key: string, vars: NodeVars): ReactNode =>
  createElement(Fragment, null, ...translateParts(lang, EN, key, vars, onMissing).map((p, i) => createElement(Fragment, { key: i }, p)))

function make(lang: Lang) {
  const tr = (key: string, vars?: Vars) => translate(lang, EN, key, vars, onMissing)
  const x = <K extends string>(key: LiteralKey<K>, vars: NodeVars) => nodes(lang, key, vars)
  return Object.assign(<K extends string>(key: LiteralKey<K>, vars?: Vars) => tr(key, vars), { tr, x })
}

/** 目前語言的翻譯。key 必須是字面字串；沒有英文就退回繁中。 */
export const t: TFn = <K extends string>(key: LiteralKey<K>, vars?: Vars) => translate(getLang(), EN, key, vars, onMissing)
/** 動態 key 的查詢（key 來自 `msg()` 標記過的常數）。 */
export const tr = (key: string, vars?: Vars): string => translate(getLang(), EN, key, vars, onMissing)
/** 標記「這個字串是要翻譯的 key」，原樣回傳。放在模組層級的標籤表，顯示時再 `tr()`。 */
export function msg<K extends string>(key: LiteralKey<K>): K { return key as unknown as K }

/** 元件用：訂閱語言，切換時重繪；回傳綁定目前語言的 `t`（含 `t.tr` 動態查詢），語言不變時身份穩定。 */
export function useT() {
  const lang = useLang()
  return useMemo(() => make(lang), [lang])
}

/** 目前語言（要自己呼叫 Intl／依語言分支時用）；切換時重繪。 */
export function useLang(): Lang {
  return useSyncExternalStore(subscribeLang, getLang, getLang)
}

export function intlLocale(lang: Lang = getLang()): string {
  return lang === 'en' ? 'en-US' : 'zh-TW'
}

/** 數字格式（千分位等）：`fmtNumber(1234.5)` → 繁中 `1,234.5`、英文 `1,234.5`；選項同 `Intl.NumberFormat`。 */
export function fmtNumber(n: number, opts?: Intl.NumberFormatOptions): string {
  return n.toLocaleString(intlLocale(), opts)
}

/** 日期時間格式；`date` 可為 Date 或毫秒。預設只給日期＋時間（到分）。 */
export function fmtDate(date: Date | number, opts: Intl.DateTimeFormatOptions = { dateStyle: 'medium', timeStyle: 'short' }): string {
  return new Intl.DateTimeFormat(intlLocale(), opts).format(date)
}

/** 除錯：英文查不到字典、退回繁中的 key（切到英文走過畫面後看漏了什麼）。mock／dev 掛在 `window.__amI18n`。 */
export function missingKeys(): string[] {
  return [...missing]
}
