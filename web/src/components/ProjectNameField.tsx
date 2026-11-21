import { useEffect, useRef, useState } from 'react'
import { useEnterCommit } from '../hooks/useEnterCommit'
import { useUnsavedGuard } from '../hooks/useUnsavedGuard'
import { isImeEnter } from '../lib/ime'
import { useStore } from '../store/store'
import { t, useT } from '../lib/i18n'
import './projectNameField.css'

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
  useT()
  const patchProject = useStore((s) => s.patchProject)
  const [draft, setDraft] = useState(label)
  const inputRef = useRef<HTMLInputElement>(null)

  const [lastLabel, setLastLabel] = useState(label)
  if (lastLabel !== label) {
    setLastLabel(label)
    if (!editing) setDraft(label)
  }

  useEffect(() => {
    if (editing) inputRef.current?.select()
  }, [editing])

  const trimmed = draft.trim()
  useUnsavedGuard(editing && trimmed !== '' && trimmed !== label)

  const commit = () => {
    onEditing(false)
    if (!trimmed || trimmed === label) {
      setDraft(label)
      return
    }
    void patchProject(projectId, { label: trimmed })
  }
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
        aria-label={t('Project 名稱')}
        size={Math.max(8, draft.length + 1)}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onClick={(e) => e.stopPropagation()}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          e.stopPropagation()
          if (e.key === 'Enter') {
            if (isImeEnter(e.nativeEvent)) return
            e.preventDefault()
            commit()
          } else if (e.key === 'Escape') {
            if (isImeEnter(e.nativeEvent)) return
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
        title={armed ? t('{name} · 點一下改名', { name: label }) : undefined}
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
    <button type="button" className="project-label-head-btn" title={t('{name} · 點一下改名', { name: label })} onClick={start}>
      <strong>{label}</strong>
    </button>
  )
}
