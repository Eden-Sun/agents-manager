import { useEffect } from 'react'

/**
 * 有未儲存的變更時，關分頁／重整／離開頁面前讓瀏覽器問一聲（`beforeunload`）。面板自己的「放棄未儲存？」只擋得住
 * 面板內的關閉，擋不住關分頁。沒有未儲存的、或元件卸載了就不留監聽。
 */
export function useUnsavedGuard(dirty: boolean): void {
  useEffect(() => {
    if (!dirty) return
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      e.preventDefault()
      // 舊瀏覽器要設 returnValue 才會跳確認；現代瀏覽器忽略內容。
      e.returnValue = ''
    }
    window.addEventListener('beforeunload', onBeforeUnload)
    return () => window.removeEventListener('beforeunload', onBeforeUnload)
  }, [dirty])
}
