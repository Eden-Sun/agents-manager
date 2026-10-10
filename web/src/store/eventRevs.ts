/**
 * 只靠事件計數通知元件重抓的兩份（`supervisor_changed`→`supervisorRev`、`bot_share_changed`→`shareRev`）：
 * 重連／resync 時事件可能漏掉，全部加一，讓開著的元件各自重抓一次。
 */
export function bumpEventRevs(s: { supervisorRev: number; shareRev: Record<string, number>; bots: readonly { id: string }[] }) {
  const shareRev = { ...s.shareRev }
  for (const b of s.bots) shareRev[b.id] = (shareRev[b.id] ?? 0) + 1
  return { supervisorRev: s.supervisorRev + 1, shareRev }
}
