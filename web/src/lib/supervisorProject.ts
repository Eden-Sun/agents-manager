export function chipTracked(
  bot: {
    pending?: boolean | null
    parent_bot_id: string | null
    project_id: string
    role?: 'manager' | 'executor' | 'advisor' | 'specialist' | 'scheduler' | null
  },
  supervisorProjectId: string | null,
): boolean {
  return (
    !bot.pending &&
    bot.parent_bot_id === null &&
    bot.role !== 'executor' &&
    bot.role !== 'specialist' &&
    bot.role !== 'advisor' &&
    bot.project_id !== supervisorProjectId
  )
}
