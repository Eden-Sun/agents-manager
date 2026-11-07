import { createElement, type ReactNode } from 'react'
import { TermLinksContent } from './TermLinksContent'

/**
 * 終端文字 → 內含可點 URL 的節點。沒有 URL 時回原字串，`<pre>` 照常顯示。
 * 終端分頁、卡住面板、shell 面板都走這裡，所以整列的分隔線在這裡一律縮短（`fitRules`），不然折行後一條線變兩三列。
 */
export function linkifyTerm(raw: string, columns?: number | null): ReactNode {
  return createElement(TermLinksContent, { raw, columns })
}
