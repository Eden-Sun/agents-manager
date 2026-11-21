const ELLIPSIS = /(?:…|\.\.\.)\s*$/
const DANGLING_SEP = /[\s]*[·|,]\s*$/

export function trimClippedTail(line: string): string {
  const s = line.trimEnd()
  if (!ELLIPSIS.test(s)) return s
  const cut = s.replace(ELLIPSIS, '').trimEnd()
  return DANGLING_SEP.test(cut) ? cut.replace(DANGLING_SEP, '').trimEnd() : cut
}
