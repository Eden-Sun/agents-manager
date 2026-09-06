import { useCallback, useEffect, useRef } from 'react'
import { useStore } from '../store/store'

/**
 * 送鍵按鈕列，blocked 的小面板與全畫面視窗共用（兩邊的答案必須一樣，不然同一個問題在兩個
 * 地方按出不同結果）。鍵名原樣送 herdr `agent.send_keys`，daemon 不翻譯。
 */
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
 * `KeyboardEvent` → herdr `agent.send_keys` 的鍵名，`null` = 這顆鍵留給瀏覽器。
 *
 * 鍵名是實測 herdr 0.8.2 的結果（`pane.send_keys` 對不認得的鍵回 `invalid_key`）：
 * - 具名鍵：`enter` / `esc` / `tab` / `backspace` / `up` / `down` / `left` / `right` / `f1`…`f12`
 * - 任何單一字元原樣送（含大寫、標點、`\`、中文），**唯獨空白要寫成 `space`**
 * - 修飾詞可疊：`ctrl+c`、`shift+tab`、`alt+enter`、`ctrl+shift+c` 都收
 * - `home` / `end` / `pageup` / `pagedown` / `delete` / `insert` **不支援** → 回 `null`，
 *   讓它們在瀏覽器裡做原本的事（捲動這個終端畫面），而不是送出去被拒絕。
 *
 * Cmd（`metaKey`）一律不攔：使用者要能用 ⌘C 複製終端上的錯誤訊息、⌘R 重新整理。
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

  // 單一字元。大小寫已經寫在字元本身（`A` 就是 shift+a），再加 `shift+` 反而是另一顆鍵。
  if (Array.from(e.key).length === 1) return `${mods}${e.key === ' ' ? 'space' : e.key}`
  return null
}

/**
 * 把按鍵送進 bot 的 pane，**依序而且合批**。
 *
 * 一顆鍵一個 `POST /keys` 的話，打字快一點就會有好幾個請求同時在路上，HTTP 不保證誰先到，
 * 送出去的字就會亂序。這裡排成一條佇列：正在送的時候按下的鍵先累積起來，下一輪一次送出
 * （`agent.send_keys` 本來就吃陣列，順序由它保證），順便把請求數壓下來。
 */
export function usePaneKeys(botId: string, onSent?: () => void) {
  const sendKeys = useStore((s) => s.sendKeys)
  const pending = useRef<string[]>([])
  const sending = useRef(false)
  const onSentRef = useRef(onSent)
  useEffect(() => {
    onSentRef.current = onSent
  })

  // 換 bot 時把還沒送出去的鍵丟掉：那些是給上一個 bot 的答案。
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
            await sendKeys(botId, pending.current.splice(0, pending.current.length))
          }
        } finally {
          sending.current = false
          onSentRef.current?.()
        }
      })()
    },
    [botId, sendKeys],
  )
}
