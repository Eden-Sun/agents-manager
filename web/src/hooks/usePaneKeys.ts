import { useCallback, useEffect, useMemo, useRef } from 'react'
import * as api from '../api'
import { KeyQueue } from '../lib/keyQueue'
import { useStore } from '../store/store'

/** blocked 小面板與全畫面視窗共用，兩邊答案才一致。鍵名原樣送 herdr，daemon 不翻譯。 */
export const KEYPAD: { label: string; keys: string[]; title: string }[] = [
  { label: 'Enter', keys: ['enter'], title: '送出 Enter' },
  { label: 'Esc', keys: ['esc'], title: '送出 Esc' },
  { label: 'y', keys: ['y'], title: '回答 y' },
  { label: 'n', keys: ['n'], title: '回答 n' },
  { label: '↑', keys: ['up'], title: '游標上移' },
  { label: '↓', keys: ['down'], title: '游標下移' },
  { label: 'ctrl+c', keys: ['ctrl+c'], title: '送出 ctrl+c' },
]

/**
 * `KeyboardEvent` → herdr 鍵名，`null` 留給瀏覽器。herdr 0.8.2 實測：單字元原樣送但空白要寫
 * `space`；修飾詞可疊；home/end/pageup/pagedown/delete/insert 不支援（回 `invalid_key`）。
 * ⌘ 不攔：要能 ⌘C 複製、⌘R 重整。
 */
export function herdrKeyFromEvent(e: KeyboardEvent): string | null {
  if (e.metaKey || e.isComposing) return null

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

  // 大小寫已在字元本身，再加 `shift+` 反而是另一顆鍵。
  if (Array.from(e.key).length === 1) return `${mods}${e.key === ' ' ? 'space' : e.key}`
  return null
}

/** 依序且合批送鍵（[`KeyQueue`]）：一鍵一個 POST 會同時在路上而亂序。 */
export function usePaneKeys(botId: string, onSent?: () => void) {
  const sendKeys = useStore((s) => s.sendKeys)
  const onSentRef = useRef(onSent)
  // ref 跨 botId 共用：換 bot 時舊的那一輪可能還在跑，每輪讀最新 botId，免得送進上一顆的 pane。
  const botIdRef = useRef(botId)
  useEffect(() => {
    onSentRef.current = onSent
    botIdRef.current = botId
  })

  const queue = useMemo(
    () => new KeyQueue((keys) => sendKeys(botIdRef.current, keys), () => onSentRef.current?.()),
    [sendKeys],
  )
  // 換 bot 丟掉未送的鍵：那是給上一個 bot 的答案。
  useEffect(() => () => queue.clear(), [botId, queue])

  return useCallback((keys: string[]) => queue.push(keys), [queue])
}

/**
 * 主機 shell 版的同一件事。shell 沒有 run／回合，所以不共用 `sendKeys`（那支要 bot id），
 * 但**順序保證必須一樣**：鍵盤同步模式下，使用者按的順序就是 pane 該收到的順序。
 */
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
        (keys) => api.sendHostShellKeys(targetRef.current.host, targetRef.current.paneId, keys),
        (e) => onSettledRef.current?.(e),
        // 貼上不能拆成鍵：文字裡的換行會變成 Enter，一段多行貼上就直接被執行了。
        (text) => api.sendHostShellText(targetRef.current.host, targetRef.current.paneId, text, false),
      ),
    [],
  )
  useEffect(() => () => queue.clear(), [host, paneId, queue])

  const press = useCallback((keys: string[]) => queue.push(keys), [queue])
  const paste = useCallback((text: string) => queue.pushText(text), [queue])
  return { press, paste }
}
