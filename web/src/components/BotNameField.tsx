import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useEnterCommit } from '../hooks/useEnterCommit'
import { useStore } from '../store/store'

/**
 * The bot's name, renamed in place. Nicknames get changed often (they are how you tell two
 * `claude`s apart mid-task), and routing that through the settings panel every time is a
 * three-click detour — so the name itself is the field: click, type, Enter.
 *
 * `PATCH /bots/:id {name}` is safe while the bot is running (docs/API.md §「bot.name 是暱稱」):
 * herdr's own agent name is derived from the bot id, not from this.
 *
 * Two placements, one behaviour:
 * - `variant="head"` (chat header): a click always starts editing.
 * - `variant="row"` (sidebar): a click starts editing **only when the row is already
 *   selected** (`armed`). On an unselected row the first click has to mean "open this bot",
 *   so the name renders as plain text and the click falls through to the row — the same
 *   click-then-click-again rename every file browser uses.
 */
export function BotNameField({
  botId,
  name,
  variant = 'head',
  armed = true,
  children,
}: {
  botId: string
  name: string
  variant?: 'head' | 'row'
  armed?: boolean
  /** Extra content that rides inside the name (persona mark, agent title …). */
  children?: ReactNode
}) {
  const patchBot = useStore((s) => s.patchBot)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(name)
  const inputRef = useRef<HTMLInputElement>(null)

  // A rename from anywhere else (settings panel, another tab) must win over a stale draft.
  const [lastName, setLastName] = useState(name)
  if (lastName !== name) {
    setLastName(name)
    if (!editing) setDraft(name)
  }

  // Deselecting the row while its name is open would strand the input; close it.
  if (editing && !armed) setEditing(false)

  useEffect(() => {
    if (editing) inputRef.current?.select()
  }, [editing])

  const trimmed = draft.trim()
  // Same rule as the settings panel: 1–32 chars, no whitespace and no `@ , : ;`.
  const valid = /^[^\s@,:;]{1,32}$/.test(trimmed)

  const commit = () => {
    setEditing(false)
    if (!valid || trimmed === name) {
      setDraft(name)
      return
    }
    void patchBot(botId, { name: trimmed })
  }
  // 手機的軟鍵盤 Enter 走 `beforeinput`，不是 keydown（Android 是 keyCode 229）。
  const enter = useEnterCommit(inputRef, commit)

  if (editing) {
    return (
      <input
        ref={inputRef}
        type="text"
        {...enter}
        className={`bot-name-input ${variant}${trimmed && !valid ? ' bad' : ''}`}
        value={draft}
        spellCheck={false}
        aria-label="Bot 名稱"
        title={valid ? '' : '1–32 字，不能有空白或 @ , : ;'}
        size={Math.max(6, draft.length + 1)}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        // The sidebar row is a listbox option and a drag source; neither may see these.
        onClick={(e) => e.stopPropagation()}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          // Stop here: ↑/↓ switch bot and Esc closes dialogs further up the tree.
          e.stopPropagation()
          if (e.key === 'Enter') {
            e.preventDefault()
            commit()
          } else if (e.key === 'Escape') {
            e.preventDefault()
            setDraft(name)
            setEditing(false)
          }
        }}
      />
    )
  }

  const start = () => {
    setDraft(name)
    setEditing(true)
  }

  if (variant === 'row') {
    // Not a <button>: the row itself is the click target for selection, and a button inside
    // an option is both invalid and un-draggable. Armed rows get the edit affordance.
    return (
      <span
        className={`bot-name${armed ? ' renamable' : ''}`}
        title={armed ? `${name} · 點一下改名` : undefined}
        onClick={
          armed
            ? (e) => {
                e.stopPropagation()
                start()
              }
            : undefined
        }
      >
        {name}
        {children}
      </span>
    )
  }

  return (
    <button type="button" className="bot-name-btn" title={`${name} · 點一下改名`} onClick={start}>
      <strong>{name}</strong>
      {children}
    </button>
  )
}
