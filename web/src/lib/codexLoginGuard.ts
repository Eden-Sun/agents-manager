import type { BotKind } from '../api/types'

export function codexLoginReplacesLogin(kind: BotKind, loggedIn: boolean | null | undefined): boolean {
  return kind === 'codex' && loggedIn === true
}
