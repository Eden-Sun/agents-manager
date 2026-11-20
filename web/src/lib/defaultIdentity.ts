import type { BotKind } from '../api/types.ts'

/**
 * 各 kind「預設帳號」（沒指定身份）的名字：claude＝`cc0`、agy＝`ag0`（使用者 2026-11-20：agy 帳號比照 cc0／cc1 叫 ag0／ag1）。
 * daemon 那邊 `ag0` 是內建身份（`tools::AGY_DEFAULT_IDENTITY`，沒有 env）：bot 的 identity 可以是空的、`ag0`、
 * 或舊資料的 `agy`，三者是同一個帳號，額度都落在裸 `agy` key。
 */
export const AGY_DEFAULT_IDENTITY = 'ag0'

/** 這個 kind 的預設帳號叫什麼；沒有固定名字的 kind 回 null。 */
export function defaultIdentityName(kind: BotKind | string | null | undefined): string | null {
  if (kind === 'claude') return 'cc0'
  if (kind === 'agy') return AGY_DEFAULT_IDENTITY
  return null
}

/** `name` 是不是這個 kind 內建的預設帳號名（agy 的 `ag0`／舊資料 `agy`）。空值不算（那是「沒指定」，由呼叫端處理）。 */
export function isBuiltinDefaultIdentity(kind: BotKind | string | null | undefined, name: string | null | undefined): boolean {
  const n = name?.trim() ?? ''
  return kind === 'agy' && (n === AGY_DEFAULT_IDENTITY || n === 'agy')
}

/** 內建預設帳號名收斂成 null（＝預設帳號），其餘原樣：比較「是不是同一個帳號」前先過這一道。 */
export function canonicalIdentity(kind: BotKind | string | null | undefined, name: string | null | undefined): string | null {
  const n = name?.trim() ?? ''
  if (!n || isBuiltinDefaultIdentity(kind, n)) return null
  return n
}
