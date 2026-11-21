import type { Dict, DictEntry, Lang, Vars } from './types'

const CJK = /[\u3400-\u9fff\uff00-\uffef\u3000-\u303f]/
const PLACEHOLDER = /\{([A-Za-z_]\w*)\}/g

export function placeholders(s: string): string[] {
  const out: string[] = []
  for (const m of s.matchAll(PLACEHOLDER)) if (!out.includes(m[1])) out.push(m[1])
  return out
}

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

export function translate(lang: Lang, dict: Dict, key: string, vars?: Vars, onMissing?: (key: string) => void): string {
  return interpolate(rawText(lang, dict, key, vars, onMissing), vars)
}

function rawText(lang: Lang, dict: Dict, key: string, vars?: Vars, onMissing?: (key: string) => void): string {
  if (lang === 'zh-TW') return key
  const entry = Object.hasOwn(dict, key) ? dict[key] : undefined
  if (entry === undefined) {
    if (CJK.test(key)) onMissing?.(key)
    return key
  }
  return pick(entry, vars)
}

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

export function entryStrings(entry: DictEntry): string[] {
  return typeof entry === 'string' ? [entry] : [entry.one, entry.other]
}

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
