import type { CodexTerminalFooter } from '../lib/codexTerminalFooter'
import { trimClippedTail } from '../lib/statusLineTail'

export function CodexTerminalFooterItems({ footer }: { footer: CodexTerminalFooter | null }) {
  if (!footer) return null
  return <>
    {footer.warnings !== null ? <span className="sl-item sl-warn" title="Codex 警告數；在終端按 F2 查看內容">⚠ {footer.warnings} {footer.warnings === 1 ? 'warning' : 'warnings'}</span> : null}
    {footer.modelLine ? <span className="statusline-text mono" title={footer.modelLine}>{trimClippedTail(footer.modelLine)}</span> : null}
  </>
}
