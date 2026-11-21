import { useCallback, useEffect, useMemo, useRef } from 'react'
import * as api from '../api'
import { KeyQueue } from '../lib/keyQueue'
import { isImeEnter } from '../lib/ime'
import { useStore } from '../store/store'
import { msg } from '../lib/i18n'

export const KEYPAD: { label: string; keys: string[]; title: string }[] = [
  { label: 'Enter', keys: ['enter'], title: msg('送出 Enter') },
  { label: 'Esc', keys: ['esc'], title: msg('送出 Esc') },
  { label: 'y', keys: ['y'], title: msg('回答 y') },
  { label: 'n', keys: ['n'], title: msg('回答 n') },
  { label: '↑', keys: ['up'], title: msg('游標上移') },
  { label: '↓', keys: ['down'], title: msg('游標下移') },
  { label: 'ctrl+c', keys: ['ctrl+c'], title: msg('送出 ctrl+c') },
  { label: '⇧←', keys: ['shift+left'], title: msg('送出 shift+←（codex：展開非同步提問）') },
  { label: '⇧→', keys: ['shift+right'], title: msg('送出 shift+→（codex：回到主輸入框）') },
]

export function herdrKeyFromEvent(e: KeyboardEvent): string | null {
  if (e.metaKey || isImeEnter(e)) return null

  const named: Record<string, string> = {
    Enter: 'enter',
    Escape: 'esc',
    Tab: 'tab',
    Backspace: 'backspace',
    ArrowUp: 'up',
    ArrowDown: 'down',
    ArrowLeft: 'left',
    ArrowRight: 'right',
  }
  const fkey = /^F([1-9]|1[0-2])$/.test(e.key) ? e.key.toLowerCase() : null
  const base = named[e.key] ?? fkey

  const mods = `${e.ctrlKey ? 'ctrl+' : ''}${e.altKey ? 'alt+' : ''}`
  if (base) return `${mods}${e.shiftKey ? 'shift+' : ''}${base}`

  if (Array.from(e.key).length === 1) return `${mods}${e.key === ' ' ? 'space' : e.key}`
  return null
}

export function usePaneKeys(botId: string, onSent?: () => void) {
  const sendKeys = useStore((s) => s.sendKeys)
  const onSentRef = useRef(onSent)
  const botIdRef = useRef(botId)
  useEffect(() => {
    onSentRef.current = onSent
    botIdRef.current = botId
  })

  const queue = useMemo(
    () => new KeyQueue((keys) => sendKeys(botIdRef.current, keys), () => onSentRef.current?.()),
    [sendKeys],
  )
  useEffect(() => () => queue.clear(), [botId, queue])

  return useCallback((keys: string[]) => queue.push(keys), [queue])
}

export function useShellKeys(host: string, paneId: string, onSettled?: (error: unknown | null) => void) {
  const targetRef = useRef({ host, paneId })
  const onSettledRef = useRef(onSettled)
  useEffect(() => {
    targetRef.current = { host, paneId }
    onSettledRef.current = onSettled
  })

  const queue = useMemo(
    () =>
      new KeyQueue(
        async (keys) => {
          await api.sendHostShellKeys(targetRef.current.host, targetRef.current.paneId, keys)
        },
        (e) => onSettledRef.current?.(e),
        async (text) => {
          await api.sendHostShellText(targetRef.current.host, targetRef.current.paneId, text, false)
        },
      ),
    [],
  )
  useEffect(() => () => queue.clear(), [host, paneId, queue])

  const press = useCallback((keys: string[]) => queue.push(keys), [queue])
  const paste = useCallback((text: string) => queue.pushText(text), [queue])
  return { press, paste }
}
