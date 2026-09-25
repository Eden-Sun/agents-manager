/**
 * 更新框要看的版本區間 `(from, to]`：changelog、分診結論與「請 AGM 解析」都用同一組（issue #561）。
 *
 * - claude：新版已經下載到磁碟，`to` 留給 daemon 讀磁碟（`null`），`from` 是 statusLine 回報的跑著的版本。
 * - codex：新版**還沒安裝**，磁碟上是舊的，daemon 讀磁碟只會拿到舊版那一段；兩個版本都寫在
 *   `update_notice` 裡（`codex_update.rs` 的三種文案），從那裡讀。codex 沒有 statusLine 版本，`running` 通常是 `null`。
 */
export function updateRange(
  kind: string,
  notice: string | null | undefined,
  running: string | null | undefined,
): { from: string | null; to: string | null } {
  const from = running || null
  if (kind !== 'codex' || !notice) return { from, to: null }
  const [a = null, b = null] = notice.match(/\d+(?:\.\d+)+/g) ?? []
  // 「codex 有新版 B（這個 run 跑的是 A），已安裝，重啟套用」：新版在前。
  if (notice.includes('這個 run 跑的是') && a && b) return { from: b, to: a }
  // 「codex 有新版 A → B，需安裝後重啟」
  if (a && b) return { from: a, to: b }
  // 「codex 有新版 B，需安裝後重啟」／「…，已安裝，重啟套用」／沒有版本
  return { from, to: a }
}
