import type { BotKind } from '../api/types'

/** 能不能建成分享用（受限）的 bot：daemon 只做了 claude、而且 workspace 在本機（SPEC「分享 bot」）。null＝可以。 */
export function shareProfileBlocked(kind: BotKind, host: string): string | null {
  if (kind !== 'claude') return '分享用的受限 bot 目前只支援 claude'
  if (host && host !== 'local') return '分享用的受限 bot 只能建在本機的專案'
  return null
}
