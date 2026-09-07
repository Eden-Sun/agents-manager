import { useEffect, useRef, useState } from 'react'
import { useStore } from '../store/store'

/**
 * The project's label, renamed in place — the same click-then-click-again gesture as
 * `BotNameField`, so the two titles in the sidebar behave alike.
 *
 * `PATCH /projects/:id {label}` is safe while bots are running (docs/API.md §3): herdr's
 * agent name is derived from the bot id, so only the legacy names and the `agent_name` slug
 * of the *next* start follow the label.
 *
 * Two placements:
 * - `variant="head"` (group chat header): a click always starts editing.
 * - `variant="row"` (sidebar): only an already-selected project arms the rename, so the
 *   first click on another project still means "open this project".
 *
 * Editing is controlled from the outside because the sidebar row is itself a `<button>`:
 * an `<input>` may not live inside one, so the parent swaps the whole row out instead.
 */
export function ProjectNameField({
  projectId,
  label,
  variant = 'head',
  armed = true,
  editing,
  onEditing,
}: {
  projectId: string
  label: string
  variant?: 'head' | 'row'
  armed?: boolean
  editing: boolean
  onEditing: (v: boolean) => void
}) {
  const patchProject = useStore((s) => s.patchProject)
  const [draft, setDraft] = useState(label)
  const inputRef = useRef<HTMLInputElement>(null)

  // A rename from anywhere else (another tab, the daemon's config) must win over a stale draft.
  const [lastLabel, setLastLabel] = useState(label)
  if (lastLabel !== label) {
    setLastLabel(label)
    if (!editing) setDraft(label)
  }

  useEffect(() => {
    if (editing) inputRef.current?.select()
  }, [editing])

  const trimmed = draft.trim()

  const commit = () => {
    onEditing(false)
    // Blank is not a rename — the daemon rejects it with a 400, so don't even ask.
    if (!trimmed || trimmed === label) {
      setDraft(label)
      return
    }
    void patchProject(projectId, { label: trimmed })
  }

  if (editing) {
    return (
      <input
        ref={inputRef}
        type="text"
        className={`project-label-input ${variant}${trimmed ? '' : ' bad'}`}
        value={draft}
        spellCheck={false}
        aria-label="Project 名稱"
        size={Math.max(8, draft.length + 1)}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        // The sidebar row is a click target for selection; it must not see these.
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
            setDraft(label)
            onEditing(false)
          }
        }}
      />
    )
  }

  const start = () => {
    setDraft(label)
    onEditing(true)
  }

  if (variant === 'row') {
    return (
      <span
        className={`project-label${armed ? ' renamable' : ''}`}
        title={armed ? `${label} · 點一下改名` : undefined}
        onClick={
          armed
            ? (e) => {
                e.stopPropagation()
                start()
              }
            : undefined
        }
      >
        {label}
      </span>
    )
  }

  return (
    <button type="button" className="project-label-head-btn" title={`${label} · 點一下改名`} onClick={start}>
      <strong>{label}</strong>
    </button>
  )
}
