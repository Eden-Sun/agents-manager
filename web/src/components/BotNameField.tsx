import { useEffect, useRef, useState } from 'react'
import { BOT_NAME_HINT, isValidBotName } from '../lib/botName'
import type { ReactNode } from 'react'
import { useEnterCommit } from '../hooks/useEnterCommit'
import { useStore } from '../store/store'

/**
 * The bot's name, renamed in place (click, type, Enter). `PATCH /bots/:id {name}` is safe while running
 * (docs/API.md §「bot.name 是暱稱」). `variant="row"` edits only when already selected (`armed`); otherwise the click selects the row.
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
  // Same rule as the daemon (`lib/botName.ts`): 1–32 chars, no `@ , : ;`, single inner spaces only.
  const valid = isValidBotName(trimmed)

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
        title={valid ? '' : BOT_NAME_HINT}
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
    // Not a <button>: a button inside a listbox option is invalid and un-draggable.
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
