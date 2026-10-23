/** 介面語言。繁中是原文（程式碼裡的字串就是繁中），英文靠 `src/i18n/en/*.ts` 的字典補。 */
export type Lang = 'zh-TW' | 'en'
/** 使用者偏好：跟隨瀏覽器，或固定一種。 */
export type LangPref = 'system' | Lang

/** 英文複數：`{n}` 為 1 用 `one`，其餘用 `other`（Intl.PluralRules('en')）。兩邊都要含同樣的 `{n}`。 */
export interface PluralEntry {
  one: string
  other: string
}
export type DictEntry = string | PluralEntry
/** 英文字典：key＝繁中原句（含 `{name}` 插值標記），value＝英文。 */
export type Dict = Readonly<Record<string, DictEntry>>
/** `t()` 的插值：`{name}` 換成 `vars.name`。複數用 `n`。 */
export type Vars = Readonly<Record<string, string | number>>

/**
 * `t()` 的 key 必須是字面字串（掃描腳本靠它抽 key）：給 `string`（非字面）型別會變 `never` 而編譯失敗。
 * 要動態 key（標籤表、依狀態挑文字）用 `msg()` 標記定義處、`tr()` 查。
 */
export type LiteralKey<K extends string> = string extends K ? never : K
export interface TFn {
  <K extends string>(key: LiteralKey<K>, vars?: Vars): string
}
