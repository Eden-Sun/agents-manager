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

/** 每個 kind 用哪個環境變數指到自己的帳號目錄（daemon `pane_identity::config_dir_var`）。 */
const HOME_VAR: Partial<Record<BotKind, string>> = { claude: 'CLAUDE_CONFIG_DIR', codex: 'CODEX_HOME', grok: 'GROK_HOME' }

/**
 * 共用預設帳號：它的 statusline 數字落在裸的 `<kind>` key 上。**跟 daemon `quota::identity_shares_default`
 * 同一條**——看 env 裡有沒有**那個 kind 的 home 變數**，不看名字。以前前端是「名字叫 cc0 或 env 整個空」：
 * 只帶 `ANTHROPIC_API_KEY` 的身分 daemon 寫裸 key、前端找分開那格；叫 cc0 卻設了 `CLAUDE_CONFIG_DIR` 的
 * daemon 寫 `claude:cc0`、前端卻去讀裸 key。
 */
export function sharesBareQuotaKey(idn: Identity): boolean {
  const v = HOME_VAR[idn.kind]
  return v ? !(v in idn.env) : Object.keys(idn.env).length === 0
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
  // cc0 優先——但前提是它真的用預設帳號（設了自己 home 變數的 cc0 有自己那一格）。
  if (ofKind.some((i) => i.name === DEFAULT_IDENTITY && sharesBareQuotaKey(i))) return DEFAULT_IDENTITY
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
  // 別的 kind 的身分（codex bot 身上的 claude `cc1`）不是這個 CLI 的帳號代號：daemon 一律寫裸 kind。
  const own = identities.find((i) => i.name === identity)
  if (own && own.kind !== kind) return kind
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
