export const MARKDOWN_MAX_CHARS = 20_000

export const MARKDOWN_MAX_QUOTE_DEPTH = 20
export const MARKDOWN_MAX_INDENT = 80

export function markdownTooLong(text: string): boolean {
  return text.length > MARKDOWN_MAX_CHARS
}

export function markdownTooDeep(text: string): boolean {
  let i = 0
  const n = text.length
  while (i < n) {
    let indent = 0
    while (i < n && (text[i] === ' ' || text[i] === '\t')) {
      indent += text[i] === '\t' ? 4 : 1
      i++
    }
    if (indent > MARKDOWN_MAX_INDENT && i < n && text[i] !== '\n') return true
    let quotes = 0
    while (i < n && (text[i] === '>' || text[i] === ' ' || text[i] === '\t')) {
      if (text[i] === '>' && ++quotes > MARKDOWN_MAX_QUOTE_DEPTH) return true
      i++
    }
    while (i < n && text[i] !== '\n') i++
    i++
  }
  return false
}
