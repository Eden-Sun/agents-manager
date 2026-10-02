import { useEffect } from 'react'
import { dialogOpen } from '../lib/dialogOpen'
import { isImeEnter } from '../lib/ime'
import { useStore } from '../store/store'

/**
 * ⌥↑/⌥↓ 從任何地方換 bot（單純 ↑/↓ 留給焦點）。在 bot 列上不動（那是排序）、有對話框開著不動。
 * 不接：Shift 組合（輸入框裡的 ⌥⇧↑/↓ 是「選取到段落頭尾」，順手換 bot 會把使用者正在選的東西丟掉）、
 * 選字中的按鍵（`isComposing`，或 WebKit 在 compositionend 之後才送的 keyCode 229）。
 */
export function useBotSwitchKeys() {
  const selectAdjacentBot = useStore((s) => s.selectAdjacentBot)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.altKey || e.metaKey || e.ctrlKey || e.shiftKey || isImeEnter(e)) return
      if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return
      if (e.defaultPrevented) return
      const target = e.target instanceof Element ? e.target : null
      if (target?.closest('.bot-row')) return
      if (dialogOpen()) return
      e.preventDefault()
      selectAdjacentBot(e.key === 'ArrowUp' ? -1 : 1)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [selectAdjacentBot])
}
