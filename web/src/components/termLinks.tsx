import type { ReactNode } from 'react'
import { TermLink } from './TermLink'
import { termPieces } from '../lib/termPieces'

/** 終端文字 → 內含可點 URL 的節點。沒有 URL 時回原字串，`<pre>` 照常顯示。 */
export function linkifyTerm(text: string, columns?: number | null): ReactNode {
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
