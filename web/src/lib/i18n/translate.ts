/** 翻譯的純函式（不碰 DOM／localStorage／全域語言狀態）。 */
import type { Dict, DictEntry, Lang, Vars } from './types'

const CJK = /[\u3400-\u9fff\uff00-\uffef\u3000-\u303f]/
const PLACEHOLDER = /\{([A-Za-z_]\w*)\}/g

/** 字串裡的 `{name}` 插值名稱（去重，依出現順序）。`{}`、`{ a }`、`{1}` 這種不是插值，原樣保留。 */
export function placeholders(s: string): string[] {
  const out: string[] = []
  for (const m of s.matchAll(PLACEHOLDER)) if (!out.includes(m[1])) out.push(m[1])
  return out
}

/** 把 `{name}` 換成 `vars.name`；`vars` 沒給到的名稱原樣留著（漏傳時畫面上看得到、不會變成 undefined）。 */
export function interpolate(s: string, vars?: Vars): string {
  if (!vars) return s
  return s.replace(PLACEHOLDER, (whole, name: string) => {
    const v = vars[name]
    return v === undefined ? whole : String(v)
  })
}

const enPlural = new Intl.PluralRules('en')

function pick(entry: DictEntry, vars?: Vars): string {
  if (typeof entry === 'string') return entry
  const n = Number(vars?.n)
  return Number.isFinite(n) && enPlural.select(n) === 'one' ? entry.one : entry.other
}

/**
 * 查翻譯並插值。繁中直接用 key 本身；英文查字典，沒有就退回繁中原句（半成品也不會出現空白或 key 名）。
 * `onMissing`：英文查不到時通知一次（除錯用，見 `index.ts` 的 `__amI18n.missing()`）。
 */
export function translate(lang: Lang, dict: Dict, key: string, vars?: Vars, onMissing?: (key: string) => void): string {
  return interpolate(rawText(lang, dict, key, vars, onMissing), vars)
}

/** 查到的句子（還沒插值）：繁中是 key 本身，英文是字典的句子，沒有就退回 key。 */
function rawText(lang: Lang, dict: Dict, key: string, vars?: Vars, onMissing?: (key: string) => void): string {
  if (lang === 'zh-TW') return key
  const entry = Object.hasOwn(dict, key) ? dict[key] : undefined
  if (entry === undefined) {
    if (CJK.test(key)) onMissing?.(key) // 純英文／符號的句子本來就不需要翻
    return key
  }
  return pick(entry, vars)
}

/**
 * 句子裡要塞元件（`<code>`、連結…）時：把 `{name}` 的位置換成 `vars.name`（任何值，含 React 節點），回傳片段陣列由呼叫端包起來。
 * 沒給到的名稱原樣留著。複數用 `n`（要是數字）。
 */
export function translateParts<V>(lang: Lang, dict: Dict, key: string, vars: Readonly<Record<string, V>>, onMissing?: (key: string) => void): (string | V)[] {
  const pluralVars = typeof vars.n === 'number' ? { n: vars.n } : undefined
  const text = rawText(lang, dict, key, pluralVars, onMissing)
  const out: (string | V)[] = []
  let last = 0
  for (const m of text.matchAll(PLACEHOLDER)) {
    if (!Object.hasOwn(vars, m[1])) continue
    if (m.index > last) out.push(text.slice(last, m.index))
    out.push(vars[m[1]])
    last = m.index + m[0].length
  }
  if (last < text.length) out.push(text.slice(last))
  return out
}

/** 一份字典裡所有英文句子（複數展開成兩句），掃描腳本用來比對插值名稱。 */
export function entryStrings(entry: DictEntry): string[] {
  return typeof entry === 'string' ? [entry] : [entry.one, entry.other]
}

/**
 * 合併各區塊的字典。同一個 key 在不同區塊重複**且英文相同**沒關係（共用字，各區各寫一份就好，不必先協調）；
 * 英文不同的回傳到 `conflicts`，由 `i18nDict.test.ts` 擋下。
 */
export function mergeDicts(parts: Readonly<Record<string, Dict>>): { dict: Dict; conflicts: { key: string; areas: string[] }[] } {
  const dict: Record<string, DictEntry> = {}
  const owner: Record<string, string> = {}
  const conflicts = new Map<string, string[]>()
  for (const [area, part] of Object.entries(parts)) {
    for (const [key, entry] of Object.entries(part)) {
      if (!Object.hasOwn(dict, key)) {
        dict[key] = entry
        owner[key] = area
      } else if (JSON.stringify(dict[key]) !== JSON.stringify(entry)) {
        const areas = conflicts.get(key) ?? [owner[key]]
        areas.push(area)
        conflicts.set(key, areas)
      }
    }
  }
  return { dict, conflicts: [...conflicts].map(([key, areas]) => ({ key, areas })) }
}
