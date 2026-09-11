import type { KeyboardEvent } from 'react'
import type { BotKind } from '../api/types'
import { useStore } from '../store/store'

/**
 * v4.0: a bot's kind, shown as a glyph or as the word (global `kindDisplay`, persisted in
 * localStorage). The `.kind-tag.<kind>` class is what the acceptance scripts key on, so it
 * stays the same in both modes; the word is always in `title` / `aria-label`.
 */

export const KIND_LABEL: Record<BotKind, string> = { claude: 'claude', codex: 'codex', grok: 'grok' }

/** Monochrome glyphs (currentColor): claude = star burst, codex = the OpenAI mark, grok = the xAI mark. */
export function KindIcon({ kind }: { kind: BotKind }) {
  switch (kind) {
    case 'claude':
      return (
        <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
          <path d="M8 1.5v13M1.5 8h13M3.4 3.4l9.2 9.2M12.6 3.4l-9.2 9.2" stroke="currentColor" strokeWidth="2" strokeLinecap="round" fill="none" />
        </svg>
      )
    case 'codex':
      // OpenAI mark — codex is the GPT CLI, so it wears the GPT logo.
      return (
        <svg viewBox="0 0 24 24" width="1em" height="1em" aria-hidden="true">
          <path
            fill="currentColor"
            d="M22.282 9.821a5.985 5.985 0 0 0-.516-4.911 6.046 6.046 0 0 0-6.51-2.9A6.065 6.065 0 0 0 4.981 4.182a5.985 5.985 0 0 0-3.998 2.9 6.046 6.046 0 0 0 .743 7.097 5.98 5.98 0 0 0 .511 4.911 6.051 6.051 0 0 0 6.515 2.9A5.985 5.985 0 0 0 13.26 24a6.056 6.056 0 0 0 5.772-4.206 5.99 5.99 0 0 0 3.998-2.9 6.056 6.056 0 0 0-.748-7.073Zm-9.022 12.608a4.476 4.476 0 0 1-2.877-1.04l.142-.081 4.778-2.758a.795.795 0 0 0 .393-.681v-6.737l2.02 1.169a.071.071 0 0 1 .038.052v5.583a4.504 4.504 0 0 1-4.494 4.493ZM3.6 18.304a4.471 4.471 0 0 1-.535-3.014l.142.085 4.783 2.758a.771.771 0 0 0 .781 0l5.843-3.368v2.332a.08.08 0 0 1-.034.062L9.74 19.95a4.499 4.499 0 0 1-6.14-1.646ZM2.341 7.896a4.485 4.485 0 0 1 2.366-1.973v5.677a.766.766 0 0 0 .388.677l5.815 3.354-2.02 1.168a.076.076 0 0 1-.071 0l-4.83-2.786a4.504 4.504 0 0 1-1.648-6.117Zm16.597 3.855-5.833-3.387 2.015-1.164a.076.076 0 0 1 .071 0l4.83 2.791a4.494 4.494 0 0 1-.676 8.105v-5.678a.79.79 0 0 0-.407-.667Zm2.01-3.023-.141-.085-4.774-2.782a.776.776 0 0 0-.785 0L9.409 9.23V6.897a.066.066 0 0 1 .028-.061l4.83-2.787a4.499 4.499 0 0 1 6.681 4.66ZM8.307 12.863l-2.02-1.164a.08.08 0 0 1-.038-.057V6.074a4.499 4.499 0 0 1 7.376-3.454l-.142.081-4.778 2.758a.795.795 0 0 0-.393.681Zm1.098-2.365 2.602-1.5 2.607 1.5v2.999l-2.598 1.5-2.607-1.5Z"
          />
        </svg>
      )
    case 'grok':
      // xAI mark: an angular X whose second diagonal is broken at the crossing.
      return (
        <svg viewBox="0 0 24 24" width="1em" height="1em" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="4.6" strokeLinecap="butt">
          <path d="M2.8 2.5 21.2 21.5" />
          <path d="M21.2 2.5 15 8.9" />
          <path d="M9 15.1 2.8 21.5" />
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
  // radio 只用 aria-checked（aria-selected 是 tab / option 的）。鍵盤照 radio 的規矩：
  // 整組一個 Tab 停點，←/→ 直接換選項——兩個選項、換了就生效，不用再按 Enter。
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(e.key)) return
    e.preventDefault()
    const next = mode === 'icon' ? 'text' : 'icon'
    setKindDisplay(next)
    e.currentTarget.querySelector<HTMLElement>(`[data-mode="${next}"]`)?.focus()
  }
  return (
    <div className="kind-display" role="radiogroup" aria-label="kind 標示方式" onKeyDown={onKeyDown}>
      <span className="kind-display-label">kind 標示</span>
      <div className="tabs small">
        <button type="button" className="tab" role="radio" data-mode="icon" aria-checked={mode === 'icon'} tabIndex={mode === 'icon' ? 0 : -1} onClick={() => setKindDisplay('icon')} title="以圖示顯示 claude / codex / grok">
          圖示
        </button>
        <button type="button" className="tab" role="radio" data-mode="text" aria-checked={mode === 'text'} tabIndex={mode === 'text' ? 0 : -1} onClick={() => setKindDisplay('text')} title="以文字顯示 claude / codex / grok">
          文字
        </button>
      </div>
    </div>
  )
}
