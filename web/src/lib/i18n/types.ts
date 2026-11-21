export type Lang = 'zh-TW' | 'en'
export type LangPref = 'system' | Lang

export interface PluralEntry {
  one: string
  other: string
}
export type DictEntry = string | PluralEntry
export type Dict = Readonly<Record<string, DictEntry>>
export type Vars = Readonly<Record<string, string | number>>

export type LiteralKey<K extends string> = string extends K ? never : K
export interface TFn {
  <K extends string>(key: LiteralKey<K>, vars?: Vars): string
}
