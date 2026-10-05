import type { ReactNode } from 'react'
import { TermLink } from './TermLink'
import { termPieces } from '../lib/termPieces'
import { fitRules } from '../lib/termRules'

/**
 * 終端文字 → 內含可點 URL 的節點。沒有 URL 時回原字串，`<pre>` 照常顯示。
 * 終端分頁、卡住面板、shell 面板都走這裡，所以整列的分隔線在這裡一律縮短（`fitRules`），不然折行後一條線變兩三列。
 */
export function linkifyTerm(raw: string, columns?: number | null): ReactNode {
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
