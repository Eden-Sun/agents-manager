/**
 * 標題列下面那條晶片列（「剛跑完」／「進行中」）要不要算這顆 bot。
 *
 * 排除規則的來源見 `UnreadChip.tsx` 檔頭：子 agent 是母 bot 自己的工人、team 成員由整隊那顆
 * 代表、總管環境底下的都是 daemon 自己的事。釘選（★）不走這裡——那是使用者自己指定的。
 *
 * 總管環境用 **`GET /api/supervisor` 的 `project_id`** 認，不是名字：那個專案是使用者可以改名的
 * （2026-09-13 已經從 `AGM` 改成 `AGM-DM-GRUP`），比對名字只是在賭他不再改名。讀不到 id 時
 * 傳 `null`——那會退回「不排除」，多顯示幾顆晶片而已，不會把使用者自己的 bot 弄不見。
 */
export function chipTracked(
  bot: { pending?: boolean | null; parent_bot_id: string | null; team?: unknown; project_id: string },
  supervisorProjectId: string | null,
): boolean {
  return !bot.pending && bot.parent_bot_id === null && !bot.team && bot.project_id !== supervisorProjectId
}
