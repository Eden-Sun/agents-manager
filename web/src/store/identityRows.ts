/**
 * 環境設定的身分列表以 `(host, name)` 為鍵（SPEC §16.2，daemon `26a14c2`）。
 *
 * 以前列表的 key、查找、用量、停用標籤都只看名字：config 同時有本機 `cc1` 與 `cc1 host="m4p"` 時，
 * 兩列都找到本機那筆、m4p 那筆在畫面上看不到，按第二列的 ✕ 刪掉的卻是**本機**的 `cc1`（第二輪 review H3）。
 */
import type { Bot, Identity, Project } from '../api/types'

const LOCAL = 'local'

/** 沒寫 host 的＝本機（跟 daemon `host_or_local` 同一條）。 */
export function identityHost(i: Pick<Identity, 'host'>): string {
  return i.host || LOCAL
}

/** React key 與查找用的鍵。 */
export function identityRowKey(i: Pick<Identity, 'host' | 'name'>): string {
  return `${identityHost(i)}:${i.name}`
}

export function findIdentity(list: readonly Identity[], host: string, name: string): Identity | undefined {
  return list.find((i) => i.name === name && identityHost(i) === (host || LOCAL))
}

/** 還綁著這一筆的 bot：只算**同一台**的（別台的 `cc1` 是別的帳號，daemon 的刪除檢查也只看同一台）。 */
export function identityUseCount(
  bots: readonly Pick<Bot, 'identity' | 'project_id'>[],
  projects: readonly Pick<Project, 'id' | 'host'>[],
  host: string,
  name: string,
): number {
  const want = host || LOCAL
  return bots.filter((b) => b.identity === name && (projects.find((p) => p.id === b.project_id)?.host || LOCAL) === want).length
}

/** 那台主機的 shell `ccN` 被 config 同名的蓋掉了嗎——只有同一台的 config 才蓋得掉。 */
export function shadowedByConfig(configured: readonly Identity[], host: string, name: string): boolean {
  return findIdentity(configured, host, name) !== undefined
}

/** 列表上的主機字樣。 */
export function hostLabel(host: string): string {
  return !host || host === LOCAL ? '本機' : host
}
