/**
 * 「這顆 bot／這個身分的額度落在哪一格」只有這一份規則。
 *
 * 以前頂端 QuotaStrip 與側欄各寫一套：一套認「名字是 cc0 **或** env 是空的」而且裸 key 有互斥
 * （一個身分認領走就不給第二個），另一套只認字面上的 `cc0`。於是 `[[identities]] name = "main"`、
 * env 留空（SPEC §16 的合法設定）時，頂端讀裸 `claude` 畫紅燈、側欄查 `claude:main` 查不到而不反灰，
 * 使用者照常派工給已經沒額度的 bot——正是側欄那段註解說要避免的畫面。
 *
 * 額度按主機分（SPEC §14），所以每次查詢都要帶 host。
 */
import { quotaKey } from '../api/types'
import type { BotKind, Identity, KindQuota, QuotaMap } from '../api/types'

/** 共用工具預設帳號的身分名。 */
export const DEFAULT_IDENTITY = 'cc0'

/** 共用預設帳號（`cc0`／env 留空）：它的 statusline 數字會落在裸的 `<kind>` key 上。 */
export function sharesBareQuotaKey(idn: Identity): boolean {
  return idn.name === DEFAULT_IDENTITY || Object.keys(idn.env).length === 0
}

/**
 * 裸 key 歸誰。只有一個身分能認領，否則兩個身分會顯示同一組數字。
 * 身分清單還沒到（側欄比 `GET /api/state` 早畫）時保底沿用 `cc0` 這條字面規則。
 *
 * 不看傳進來的順序：頂端照字母排過、側欄是 config 原順序，兩個都不叫 cc0 的空 env 身分
 * 以前會各挑到不同人（第二輪 review M1）。
 */
export function bareQuotaOwner(identities: readonly Identity[], kind: BotKind): string | null {
  const ofKind = identities.filter((i) => i.kind === kind)
  if (ofKind.length === 0) return DEFAULT_IDENTITY
  if (ofKind.some((i) => i.name === DEFAULT_IDENTITY)) return DEFAULT_IDENTITY
  const sharing = ofKind.filter(sharesBareQuotaKey).map((i) => i.name).sort()
  return sharing[0] ?? null
}

/** 落點的 key（不含 host 前綴）：有自己那一格就用自己的，否則只有裸 key 的主人退回裸 key。 */
export function quotaBaseKey(
  quota: QuotaMap,
  host: string,
  kind: BotKind,
  identity: string | null,
  identities: readonly Identity[],
): string {
  if (!identity) return kind
  const keyed = `${kind}:${identity}`
  if (quota[quotaKey(host, keyed)] != null) return keyed
  const bare = quotaKey(host, kind)
  return bareQuotaOwner(identities, kind) === identity && bare in quota ? kind : keyed
}

/** 落點上的讀數；那一格還沒有數字就是 null。 */
export function quotaForIdentity(
  quota: QuotaMap,
  host: string,
  kind: BotKind,
  identity: string | null,
  identities: readonly Identity[],
): KindQuota | null {
  return quota[quotaKey(host, quotaBaseKey(quota, host, kind, identity, identities))] ?? null
}
