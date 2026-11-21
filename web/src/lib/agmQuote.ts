
const AGM_COLON_RE = /^\s*(AGM|AG ?Man)\b[^\n]{0,24}?[：:]/
const AGM_BRACKET_RE = /^\s*\[\s*(AGM|AG ?Man)\b[^\]\n]{0,60}\]/
const FROM_BRACKET_RE = /^\s*\[\s*來自\s*([^/\]\n]{1,40}?)\s*(?:\/[^\]\n]{0,60})?\]/

export function quotedFrom(content: string): string | null {
  const first = content.trimStart().split('\n', 1)[0] ?? ''
  const named = FROM_BRACKET_RE.exec(first)
  if (named) return named[1]
  if (AGM_COLON_RE.test(first) || AGM_BRACKET_RE.test(first)) return 'AGM'
  return null
}

export function isAgmQuote(content: string): boolean {
  return quotedFrom(content) === 'AGM'
}
