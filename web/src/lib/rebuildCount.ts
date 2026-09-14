/**
 * 「集滿幾個重建申請就不等整點」的計數規則（使用者 2026-09-14）。
 *
 * 跟 `scripts/ops/daemon-update-kick.sh` 同一套：`purpose=rebuild`、狀態還沒被決定掉
 * （`pending` / `approved`），同一個 requester 對同一個 commit 只算一筆。`denied` /
 * `revoked` / `consumed` 不算——那些不是還在等的申請。
 *
 * 差別只有一個，寫在 `docs/UI-DECISIONS.md`：腳本還會丟掉「上次上線之前建立」的申請
 * （它讀得到 `daemon-update.built` 的 mtime），瀏覽器讀不到那個檔，所以沒被決定掉的舊申請
 * 在這裡仍然算一筆。
 */
export interface RebuildRequest {
  id: string
  requester: string
  target_commit: string
  scope: string
  status: string
  created_at: string
}

/** 還在等的那幾筆（新的在前），已去重。 */
export function pendingRebuilds(rows: RebuildRequest[]): RebuildRequest[] {
  const seen = new Set<string>()
  const out: RebuildRequest[] = []
  for (const r of rows) {
    if (r.status !== 'pending' && r.status !== 'approved') continue
    const key = `${r.requester} ${r.target_commit}`
    if (seen.has(key)) continue
    seen.add(key)
    out.push(r)
  }
  return out.sort((a, b) => b.created_at.localeCompare(a.created_at))
}
