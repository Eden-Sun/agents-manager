/**
 * 「集滿幾個重建申請、或最早一筆等太久，就不等整點」的計數規則（使用者 2026-09-14、09-15）。
 *
 * 跟 `scripts/ops/daemon-update-kick.sh` 同一套：`purpose=rebuild`、狀態還沒被決定掉
 * （`pending` / `approved`）、建立時間晚於上次成功上線，同一個 requester 對同一個 commit
 * 只算一筆。`denied` / `revoked` / `consumed` 不算——那些不是還在等的申請。
 *
 * 上線時間來自 `GET /api/supervisor` 的 `last_deploy.at`（daemon 960ba06 起）。舊 daemon 沒有
 * 這個欄位，`since` 就是 `null`：不濾時間，寧可多算一筆也不要把真的在等的申請藏起來。
 */
export interface RebuildRequest {
  id: string
  requester: string
  target_commit: string
  scope: string
  status: string
  created_at: string
}

/** 還在等的那幾筆（新的在前），已去重。`since` = 上次上線時間（RFC3339），`null` = 不濾。 */
export function pendingRebuilds(rows: RebuildRequest[], since?: string | null): RebuildRequest[] {
  const cut = since ? Date.parse(since) : NaN
  const seen = new Set<string>()
  const out: RebuildRequest[] = []
  for (const r of rows) {
    if (r.status !== 'pending' && r.status !== 'approved') continue
    // 時間解析不出來的（壞格式）一律留著：漏算會讓 chip 少一筆，多算只是早一點提醒。
    if (!Number.isNaN(cut)) {
      const at = Date.parse(r.created_at)
      if (!Number.isNaN(at) && at <= cut) continue
    }
    const key = `${r.requester} ${r.target_commit}`
    if (seen.has(key)) continue
    seen.add(key)
    out.push(r)
  }
  return out.sort((a, b) => b.created_at.localeCompare(a.created_at))
}

/** 最早那筆還在等的申請等了幾分鐘（沒有、或時間壞掉就是 0）。`now` 給測試用。 */
export function oldestWaitMinutes(rows: RebuildRequest[], now: number = Date.now()): number {
  const times = rows.map((r) => Date.parse(r.created_at)).filter((t) => !Number.isNaN(t))
  if (times.length === 0) return 0
  return Math.max(0, Math.floor((now - Math.min(...times)) / 60_000))
}
