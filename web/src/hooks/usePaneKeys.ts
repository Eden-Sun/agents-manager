import { useCallback, useEffect, useRef } from 'react'
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

/** 依序且合批送鍵：一鍵一個 POST 會同時在路上而亂序，送的期間累積、下一輪一次送。 */
export function usePaneKeys(botId: string, onSent?: () => void) {
  const sendKeys = useStore((s) => s.sendKeys)
  const pending = useRef<string[]>([])
  const sending = useRef(false)
  const onSentRef = useRef(onSent)
  // ref 跨 botId 共用：換 bot 時舊 while 可能還在跑，每輪讀最新 botId，免得送進上一顆的 pane。
  const botIdRef = useRef(botId)
  useEffect(() => {
    onSentRef.current = onSent
    botIdRef.current = botId
  })

  // 換 bot 丟掉未送的鍵：那是給上一個 bot 的答案。
  useEffect(() => {
    return () => {
      pending.current = []
    }
  }, [botId])

  return useCallback(
    (keys: string[]) => {
      pending.current.push(...keys)
      if (sending.current) return
      void (async () => {
        sending.current = true
        try {
          while (pending.current.length) {
            await sendKeys(botIdRef.current, pending.current.splice(0, pending.current.length))
          }
        } finally {
          sending.current = false
          onSentRef.current?.()
        }
      })()
    },
    [sendKeys],
  )
}
