import type { BotKind } from '../api/types'
import { useStore } from '../store/store'

/**
 * v4.0: a bot's kind, shown as a glyph or as the word (global `kindDisplay`, persisted in
 * localStorage). The `.kind-tag.<kind>` class is what the acceptance scripts key on, so it
 * stays the same in both modes; the word is always in `title` / `aria-label`.
 */

export const KIND_LABEL: Record<BotKind, string> = { claude: 'claude', codex: 'codex', grok: 'grok' }

/** Monochrome glyphs (currentColor): claude = star burst, codex = `>_` box, grok = X. */
export function KindIcon({ kind }: { kind: BotKind }) {
  switch (kind) {
    case 'claude':
      return (
        <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
          <path d="M8 1.5v13M1.5 8h13M3.4 3.4l9.2 9.2M12.6 3.4l-9.2 9.2" stroke="currentColor" strokeWidth="2" strokeLinecap="round" fill="none" />
        </svg>
      )
    case 'codex':
      return (
        <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
          <rect x="1.5" y="2.5" width="13" height="11" rx="2" fill="none" stroke="currentColor" strokeWidth="1.5" />
          <path d="M4.5 6l2.5 2-2.5 2M8.5 10.5h3" stroke="currentColor" strokeWidth="1.5" fill="none" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      )
    case 'grok':
      return (
        <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
          <path d="M3 3l10 10M13 3L3 13" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" fill="none" />
        </svg>
      )
  }
}

export function KindTag({ kind, title, className }: { kind: BotKind; title?: string; className?: string }) {
  const mode = useStore((s) => s.kindDisplay)
  return (
    <span
      className={`kind-tag ${kind}${mode === 'icon' ? ' icon' : ''}${className ? ` ${className}` : ''}`}
      title={title ?? KIND_LABEL[kind]}
      aria-label={KIND_LABEL[kind]}
      role="img"
    >
      {mode === 'icon' ? <KindIcon kind={kind} /> : KIND_LABEL[kind]}
    </span>
  )
}

/** The icon / text switch (sidebar foot). */
export function KindDisplayToggle() {
  const mode = useStore((s) => s.kindDisplay)
  const setKindDisplay = useStore((s) => s.setKindDisplay)
  return (
    <div className="kind-display" role="radiogroup" aria-label="kind 標示方式">
      <span className="kind-display-label">kind 標示</span>
      <div className="tabs small">
        <button type="button" className="tab" role="radio" aria-checked={mode === 'icon'} aria-selected={mode === 'icon'} onClick={() => setKindDisplay('icon')} title="以圖示顯示 claude / codex / grok">
          圖示
        </button>
        <button type="button" className="tab" role="radio" aria-checked={mode === 'text'} aria-selected={mode === 'text'} onClick={() => setKindDisplay('text')} title="以文字顯示 claude / codex / grok">
          文字
        </button>
      </div>
    </div>
  )
}
