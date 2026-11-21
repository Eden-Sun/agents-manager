import { createElement, type ReactNode } from 'react'
import { TermLinksContent } from './TermLinksContent'

export function linkifyTerm(raw: string, columns?: number | null): ReactNode {
  return createElement(TermLinksContent, { raw, columns })
}
