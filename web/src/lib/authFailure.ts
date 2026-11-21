import type { Message } from '../api/types'

const HOOK_AUTH_LABEL = '（帳號或授權）'
const NOT_LOGGED_IN = /not logged in\s*·\s*(?:please\s+)?run \/login/i
const ONLY_NOT_LOGGED_IN = /^[⎿\s]*not logged in\s*·\s*(?:please\s+)?run \/login[.。]?\s*$/i

type Msg = Pick<Message, 'role' | 'content'>

export function isAuthFailure(msg: Msg): boolean {
  if (msg.role === 'system') return msg.content.includes(HOOK_AUTH_LABEL) || NOT_LOGGED_IN.test(msg.content)
  if (msg.role === 'assistant') return ONLY_NOT_LOGGED_IN.test(msg.content)
  return false
}

export function authActionTargetId(list: readonly (Msg & Pick<Message, 'id'>)[]): string | null {
  for (let i = list.length - 1; i >= 0; i -= 1) {
    const m = list[i]
    if (isAuthFailure(m)) return m.id
    if (m.role === 'assistant') return null
  }
  return null
}
