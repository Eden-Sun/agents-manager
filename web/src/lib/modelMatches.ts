export function stripModelContext(model: string | null | undefined): string {
  if (!model) return ''
  return model.split('[')[0].trim().toLowerCase()
}

export function modelMatches(a: string | null | undefined, b: string | null | undefined): boolean {
  const normA = stripModelContext(a)
  const normB = stripModelContext(b)
  if (!normA || !normB) return false
  if (normA === normB) return true
  const isFamily = (alias: string, full: string) =>
    !alias.includes('-') && full.startsWith(`claude-${alias}-`)
  return isFamily(normA, normB) || isFamily(normB, normA)
}
