import { ApiError } from '../api/types'

export function isGhAuthError(e: unknown): boolean {
  if (!(e instanceof ApiError) || e.status !== 502) return false
  const m = e.message.toLowerCase()
  return m.includes('未登入') || m.includes('auth login') || m.includes('authentication') || m.includes('401')
}
