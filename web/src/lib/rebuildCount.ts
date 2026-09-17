/**
 * 「集滿幾個重建申請、或最早一筆等太久，就不等整點」的計數規則（使用者 2026-09-14、09-15）。
 *
 * 跟 `scripts/ops/daemon-update-kick.sh` 同一套：`purpose=rebuild`、狀態還沒被決定掉
 * （`pending` / `approved`）、**還沒過期**、建立時間晚於上次成功上線，同一個 requester 對同一個 commit
 * 只算一筆，而且**不算腳本自己提的那筆**。`denied` / `revoked` / `consumed` 不算——那些不是還在等的申請。
 *
 * 過期與腳本自己那兩條是 5d3d062 只加進腳本、web 沒跟上的（review 2026-09-16 c2 L2）：daemon 不會把過期的
 * 核准改狀態，所以兩小時前那筆 `--expires-in 3600` 的申請在表上仍是 `pending`；腳本不算它，chip 卻照樣轉警示色
 * 說「下一輪檢查就會安排重建」——那不是事實。
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
  /** RFC3339；沒有就是不會過期。 */
  expires_at?: string | null
}

/**
 * 例行更新腳本自己的申請不算（`scripts/ops/daemon-update-kick.sh` 的 `AM_AGENT_NAME` 預設值）。
 * 算進去的話：它自己申請、30 分鐘後自己觸發「等太久」，每 5 分鐘一輪就叫醒協調者一次。
 */
export const KICK_REQUESTER = 'daemon-update-kick'

/** 還在等的那幾筆（新的在前），已去重。`since` = 上次上線時間（RFC3339），`null` = 不濾。`now` 給測試用。 */
export function pendingRebuilds(rows: RebuildRequest[], since?: string | null, now: number = Date.now()): RebuildRequest[] {
  const cut = since ? Date.parse(since) : NaN
  const seen = new Set<string>()
  const out: RebuildRequest[] = []
  for (const r of rows) {
    if (r.status !== 'pending' && r.status !== 'approved') continue
    // 過期的沒有人會拿去 acquire 窗口，腳本也不算它。時間壞掉的留著（跟下面同一個取捨）。
    if (r.expires_at) {
      const exp = Date.parse(r.expires_at)
      if (!Number.isNaN(exp) && exp <= now) continue
    }
    if (r.requester === KICK_REQUESTER) continue
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
