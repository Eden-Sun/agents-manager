export function splitAtCursor(text: string): { before: string; gap: string; after: string } {
  const lines = text.split('\n')
  let last = lines.length - 1
  while (last >= 0 && !lines[last].trim()) last--
  if (last < 0) return { before: '', gap: '', after: text }
  const line = lines[last].replace(/\s+$/, '')
  const before = [...lines.slice(0, last), line].join('\n')
  const after = lines.length - 1 > last ? '\n' + lines.slice(last + 1).join('\n') : ''
  const gap = /[%$#>❯]$/.test(line) ? ' ' : ''
  return { before, gap, after }
}
