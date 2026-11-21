import type { BotKind } from '../api/types.ts'

export const AGY_DEFAULT_IDENTITY = 'ag0'

export function defaultIdentityName(kind: BotKind | string | null | undefined): string | null {
  if (kind === 'claude') return 'cc0'
  if (kind === 'agy') return AGY_DEFAULT_IDENTITY
  return null
}

export function isBuiltinDefaultIdentity(kind: BotKind | string | null | undefined, name: string | null | undefined): boolean {
  const n = name?.trim() ?? ''
  return kind === 'agy' && (n === AGY_DEFAULT_IDENTITY || n === 'agy')
}

export function canonicalIdentity(kind: BotKind | string | null | undefined, name: string | null | undefined): string | null {
  const n = name?.trim() ?? ''
  if (!n || isBuiltinDefaultIdentity(kind, n)) return null
  return n
}
