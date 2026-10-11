import { useEffect } from 'react'

/**
 * 沒有 drop 區接手的檔案拖放：擋掉瀏覽器「在這個分頁打開檔案」的預設行為。
 * 在 window 上聽（bubble 階段，晚於 React 的 `#root`），所以 `useDropTarget` 接手的事件到這裡已經 `defaultPrevented`，不會被改寫。
 */
export function guardFileDrop(e: Pick<DragEvent, 'type' | 'dataTransfer' | 'defaultPrevented' | 'preventDefault'>): void {
  if (e.defaultPrevented) return // useDropTarget 已經接了
  if (!Array.from(e.dataTransfer?.types ?? []).includes('Files')) return // 文字、連結、側欄拖曳排序照舊
  e.preventDefault()
  if (e.type === 'dragover' && e.dataTransfer) e.dataTransfer.dropEffect = 'none'
}

export function useFileDropGuard(): void {
  useEffect(() => {
    window.addEventListener('dragover', guardFileDrop)
    window.addEventListener('drop', guardFileDrop)
    return () => {
      window.removeEventListener('dragover', guardFileDrop)
      window.removeEventListener('drop', guardFileDrop)
    }
  }, [])
}
