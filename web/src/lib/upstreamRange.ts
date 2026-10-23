/**
 * agy、grok 的更新框要先看「這次更新改了什麼」（#1930）。落後的主機可能不只一台、各自停在不同版本：
 * changelog 取最舊那台到新版的區間，這樣每一台漏掉的條目都看得到。
 * 任何一台版本讀不到（或不是純數字版本）就不知道區間下限，一律傳 `null`，後端只給新版那一段，不猜。
 */
import { cmpVersion } from './releaseTriage'

export interface RangeHost {
  host: string
  from: string | null
}

const plain = (v: string | null): v is string => v !== null && /^\d+(\.\d+)*$/.test(v.trim())

/** 要拿去問 changelog 的主機與起點版本。沒有主機時退回 `local`、`null`。 */
export function changelogRange(hosts: RangeHost[]): RangeHost {
  if (hosts.length === 0) return { host: 'local', from: null }
  if (!hosts.every((h) => plain(h.from))) return { host: hosts[0].host, from: null }
  const oldest = hosts.reduce((a, b) => (cmpVersion(b.from!.trim(), a.from!.trim()) < 0 ? b : a))
  return { host: oldest.host, from: oldest.from!.trim() }
}
