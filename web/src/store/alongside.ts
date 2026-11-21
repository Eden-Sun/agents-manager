export interface AlongsideIO {
  sendText: (botId: string, text: string, enter: boolean, record: boolean) => Promise<boolean>
}

export async function typeAlongside(io: AlongsideIO, botId: string, body: string): Promise<boolean> {
  const text = body.trim()
  if (!text) return false
  return io.sendText(botId, text, true, true)
}
