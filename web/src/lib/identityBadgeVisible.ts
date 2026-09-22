import type { BotKind, Identity } from '../api/types'

/**
 * 側欄那顆身份晶片（`cc1` / `預設` / `未知`）要不要畫（2026-09-22 使用者：「codex 只有一個帳號就不用 show，
 * 是 claude 有複數個才要」）。
 *
 * 晶片存在的理由是「同一個 CLI 有兩個帳號時分得出來」；那個 kind 在這台主機上只有一個帳號時，
 * 畫出來的只有雜訊——尤其 run 還沒回報身份時畫「未知」，看了也無事可做。
 *
 * 算的是**這台主機上這個 kind 有幾個可選的帳號**：該主機認得的身份（config＋shell 的 `ccN`）加上
 * 「不指定」那一個（沒有任何具名身份時，不指定就是唯一選擇）。兩個以上才畫。
 * bot 身上已經寫著的身份也算進去：設定裡被刪掉、但 bot 還綁著時，照樣要看得到它跟別人不同。
 */
export function identityBadgeVisible(
  kind: BotKind,
  hostIdentities: readonly Pick<Identity, 'name' | 'kind'>[],
  usedNames: readonly (string | null | undefined)[] = [],
): boolean {
  const names = new Set<string>()
  for (const i of hostIdentities) if (i.kind === kind) names.add(i.name)
  for (const n of usedNames) if (n) names.add(n)
  // 「不指定（預設）」也是一個選項：有具名身份時它才是第二個可能。
  const choices = names.size + (names.size > 0 ? 1 : 0)
  return choices >= 2
}
