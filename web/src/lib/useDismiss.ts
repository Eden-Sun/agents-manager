import { useEffect, useRef, type RefObject } from 'react'

/**
 * 彈出框／選單共用 hook：點擊外圍或按 Escape 鍵時觸發 close。
 * - 只在 open 時掛載 document 的 mousedown 與 keydown 監聽
 * - 點擊目標若不在 ref 元素內則關閉
 * - 按 Escape 鍵則關閉
 * - 關閉或元件卸載時移除監聽
 */
export function useDismiss<T extends Element = Element>(
  open: boolean,
  ref: RefObject<T | null>,
  close: () => void,
): void {
  const closeRef = useRef(close)
  useEffect(() => {
    closeRef.current = close
  })

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) closeRef.current()
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') closeRef.current()
    }
    document.addEventListener('mousedown', onDoc)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDoc)
      document.removeEventListener('keydown', onKey)
    }
  }, [open, ref])
}
