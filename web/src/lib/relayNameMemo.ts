const seen = new Map<string, string>()

export function rememberRelayName(id: string, name: string): void {
  if (id && name) seen.set(id, name)
}

export function recallRelayName(id: string): string | undefined {
  return seen.get(id)
}
