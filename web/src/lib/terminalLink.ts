export function openTerminalLink(uri: string): void {
  let protocol: string
  try {
    protocol = new URL(uri).protocol
  } catch {
    return
  }
  if (protocol !== 'http:' && protocol !== 'https:') return
  window.open(uri, '_blank', 'noopener,noreferrer')
}
