import { useEffect, useRef, useState } from 'react'
import { useEnterCommit } from '../hooks/useEnterCommit'
import { useStore } from '../store/store'
import './projectNameField.css'

/**
 * In-place project rename, same gesture as `BotNameField`; safe while bots run (docs/API.md §3).
 * `variant="row"`: only an already-selected project arms the rename. Editing is controlled from
 * outside because the sidebar row is a `<button>` and cannot contain an `<input>`.
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
    if (!trimmed || trimmed === label) {
      setDraft(label)
      return
    }
    void patchProject(projectId, { label: trimmed })
  }
  // 手機的軟鍵盤 Enter 不走 keydown，見 `useEnterCommit`。
  const enter = useEnterCommit(inputRef, commit)

  if (editing) {
    return (
      <input
        ref={inputRef}
        {...enter}
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
