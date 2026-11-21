import { ApiError } from '../api/types'

export function isLostCursor(e: unknown): boolean {
  if (!(e instanceof ApiError) || e.status !== 404) return false
  const reason = e.body.reason
  return reason === 'before_message_gone' || reason === 'before_message_not_in_conversation'
}

const recovering = new Set<string>()

export async function recoverLostCursor(key: string, e: unknown, reload: () => Promise<void>, again: () => Promise<void>): Promise<boolean> {
  if (!isLostCursor(e) || recovering.has(key)) return false
  recovering.add(key)
  try {
    await reload()
    await again()
  } finally {
    recovering.delete(key)
  }
  return true
}
