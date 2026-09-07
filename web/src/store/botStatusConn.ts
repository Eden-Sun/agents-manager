/**
 * Which connection flag a `bot_status` frame's `connected` field is allowed to touch (#20).
 *
 * `bot_status.connected` is the connection state of the host/session the bot lives on
 * (daemon `App::bot_connected`), not the local herdr link. Only a local, non-default-session
 * bot may write the global `connected`; a local default-session bot writes `defaultConnected`;
 * a remote bot only patches its own `hosts[name].connected`.
 */
export type BotStatusConnTarget =
  | { kind: 'none' }
  | { kind: 'global'; connected: boolean }
  | { kind: 'default'; connected: boolean }
  | { kind: 'host'; host: string; connected: boolean }

export function botStatusConnTarget(input: {
  connected: boolean | undefined
  /** `host` from the frame, falling back to the bot's project host. */
  host: string
  defaultSession: boolean
}): BotStatusConnTarget {
  if (input.connected === undefined) return { kind: 'none' }
  const host = input.host || 'local'
  if (host !== 'local') return { kind: 'host', host, connected: input.connected }
  if (input.defaultSession) return { kind: 'default', connected: input.connected }
  return { kind: 'global', connected: input.connected }
}
