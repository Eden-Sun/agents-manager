import { useEffect, useRef, useState } from 'react'
import { BOT_NAME_HINT, isValidBotName } from '../lib/botName'
import { isImeEnter } from '../lib/ime'
import type { ReactNode } from 'react'
import { useEnterCommit } from '../hooks/useEnterCommit'
import { useStore } from '../store/store'
import { useT } from '../lib/i18n'

export function BotNameField({
  botId,
  name,
  variant = 'head',
  armed = true,
  hint,
  children,
}: {
  botId: string
  name: string
  variant?: 'head' | 'row'
  armed?: boolean
  hint?: string
  children?: ReactNode
}) {
  const t = useT()
  const patchBot = useStore((s) => s.patchBot)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(name)
  const inputRef = useRef<HTMLInputElement>(null)

  const [lastName, setLastName] = useState(name)
  if (lastName !== name) {
    setLastName(name)
    if (!editing) setDraft(name)
  }

  if (editing && !armed) setEditing(false)

  useEffect(() => {
    if (editing) inputRef.current?.select()
  }, [editing])

  const trimmed = draft.trim()
  const valid = isValidBotName(trimmed)

  const commit = () => {
    setEditing(false)
    if (!valid || trimmed === name) {
      setDraft(name)
      return
    }
    void patchBot(botId, { name: trimmed })
  }
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
        aria-label={t('Bot 名稱')}
        title={valid ? '' : t.tr(BOT_NAME_HINT)}
        size={Math.max(6, draft.length + 1)}
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
    return (
      <span
        className={`bot-name${armed ? ' renamable' : ''}`}
        title={armed || hint ? t('{name}{rename}{hint}', { name, rename: armed ? ` · ${t('點一下改名')}` : '', hint: hint ? `\n${hint}` : '' }) : undefined}
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
    <button type="button" className="bot-name-btn" title={t('{name} · 點一下改名', { name })} onClick={start}>
      <strong>{name}</strong>
      {children}
    </button>
  )
}
