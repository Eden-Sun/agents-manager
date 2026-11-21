export type BotStatusConnTarget =
  | { kind: 'none' }
  | { kind: 'global'; connected: boolean }
  | { kind: 'default'; connected: boolean }
  | { kind: 'host'; host: string; connected: boolean }

export function botStatusConnTarget(input: {
  connected: boolean | undefined
  host: string
  defaultSession: boolean
}): BotStatusConnTarget {
  if (input.connected === undefined) return { kind: 'none' }
  const host = input.host || 'local'
  if (host !== 'local') return { kind: 'host', host, connected: input.connected }
  if (input.defaultSession) return { kind: 'default', connected: input.connected }
  return { kind: 'global', connected: input.connected }
}
