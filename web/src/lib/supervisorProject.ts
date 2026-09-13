/**
 * 這個專案是不是「總管自己的環境」（daemon 建的，不是使用者的專案）。
 *
 * daemon 建的總管專案叫 `AGM`（`supervisor::setup`），但總管開出去做雜務的 bot 會被放進
 * `AGM-…` 開頭的另一個專案（2026-09-13 實機：`agm-pxf2pv-browser-gc` 在 `AGM-DM-GRUP` 底下）。
 * 只比對 `=== 'AGM'` 的話那些雜務 bot 跑完就會出現在標題列下面那條「剛跑完」——那不是使用者
 * 派的工作，他不需要知道它跑完了。
 *
 * 只認**前綴**而不是「名字裡有 AGM」：使用者自己的專案叫 `my-AGM-tools` 不該被吃掉。
 */
export function isSupervisorProject(label: string): boolean {
  return label === 'AGM' || label.startsWith('AGM-')
}

/**
 * 這顆 bot 要不要出現在標題列下面那條晶片列（「剛跑完」／「進行中」）。
 *
 * 排除規則的來源見 `UnreadChip.tsx` 檔頭：子 agent 是母 bot 自己的工人、team 成員由整隊那顆
 * 代表、總管環境底下的都是 daemon 自己的事。釘選（★）不走這裡——那是使用者自己指定的。
 */
export function chipTracked(
  bot: { pending?: boolean | null; parent_bot_id: string | null; team?: unknown; project_id: string },
  supervisorProjectIds: readonly string[],
): boolean {
  return (
    !bot.pending &&
    bot.parent_bot_id === null &&
    !bot.team &&
    !supervisorProjectIds.includes(bot.project_id)
  )
}
