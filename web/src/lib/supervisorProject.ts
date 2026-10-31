/**
 * 晶片列算不算這顆 bot（排除規則見 `UnreadChip.tsx`）。總管環境用 `project_id` 認不用名字
 * （2026-09-13 已被改名成 `AGM-DM-GRUP`）；讀不到傳 `null`＝不排除。
 * 專家模式的執行者／顧問取代了 child（沒有 parent_bot_id），一樣不進晶片列；管理者照算。
 */
export function chipTracked(
  bot: {
    pending?: boolean | null
    parent_bot_id: string | null
    project_id: string
    role?: 'manager' | 'executor' | 'advisor' | null
  },
  supervisorProjectId: string | null,
): boolean {
  return (
    !bot.pending &&
    bot.parent_bot_id === null &&
    bot.role !== 'executor' &&
    bot.role !== 'advisor' &&
    bot.project_id !== supervisorProjectId
  )
}
