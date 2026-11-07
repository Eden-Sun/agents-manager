import type { ReactNode } from 'react'
import { TermLink } from './TermLink'
import { termPieces } from '../lib/termPieces'
import { fitRules } from '../lib/termRules'

export function TermLinksContent({ raw, columns }: { raw: string; columns?: number | null }): ReactNode {
  const text = fitRules(raw)
  if (!/https?:\/\//.test(text)) return text
  const out: ReactNode[] = []
  termPieces(text, columns).forEach((row, i) => {
    if (i > 0) out.push('\n')
    for (const p of row) {
      if (p.url) out.push(<TermLink key={`${i}:${out.length}`} url={p.url} text={p.text} />)
      else out.push(p.text)
    }
  })
  return out
}
