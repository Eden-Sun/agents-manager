import { ApiError } from '../api/types'

export interface AgmDeleteAsk {
  botId: string
  name: string
  role: string | null
}

export function supervisorOwnedAsk(e: unknown, botId: string, fallbackName: string): AgmDeleteAsk | null {
  if (!(e instanceof ApiError) || e.status !== 409 || e.body.reason !== 'supervisor_owned') return null
  const body = e.body as Record<string, unknown>
  const name = typeof body.name === 'string' && body.name.trim() ? body.name : fallbackName
  const role = typeof body.role === 'string' && body.role.trim() ? body.role : null
  return { botId, name, role }
}
