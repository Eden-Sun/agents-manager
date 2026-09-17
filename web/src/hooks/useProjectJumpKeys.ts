import { useEffect } from 'react'
import { orderedProjects, useStore } from '../store/store'

/** 1–9：`Digit1`…`Digit9`（看實體鍵位，不看輸入法或 Shift 打出什麼字）。 */
function slotOf(code: string): number | null {
  const m = /^Digit([1-9])$/.exec(code)
  return m ? Number(m[1]) - 1 : null
}

/**
 * 第 n 個＝**側欄畫出來的**第 n 個（`shown`：側欄 `.project[data-project-id]` 的順序）。搜尋時沒命中的專案整塊不畫，
 * 照全部專案的順序算會選到一個被藏起來的專案，主面板換掉、側欄卻捲不到它（review3 c5 L4）。
 * 側欄根本不在畫面上（`shown === null`）才退回全部專案的順序。
 */
export function jumpTargetId(slot: number, shown: readonly string[] | null, ordered: readonly { id: string }[]): string | null {
  return (shown ? shown[slot] : ordered[slot]?.id) ?? null
}

/**
 * Control+1…9 跳到側欄第 n 個專案的群組對話並把游標放進輸入框（2026-09-16 使用者）。
 *
 * 用 Control 不用 ⌘：macOS 的 ⌘1…9 是瀏覽器換分頁，攔不下來也不該攔。`code` 認實體鍵位，中文輸入法照樣有效；
 * 對話框開著、正在組字、或已經被別人處理掉（`defaultPrevented`）就不動。
 */
export function useProjectJumpKeys() {
  const selectProject = useStore((s) => s.selectProject)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.ctrlKey || e.metaKey || e.altKey || e.shiftKey || e.isComposing || e.defaultPrevented) return
      const slot = slotOf(e.code)
      if (slot === null) return
      if (document.querySelector('.modal-backdrop, .confirm-backdrop')) return
      const nav = document.querySelector('.sidebar-scroll')
      const shown = nav ? [...nav.querySelectorAll<HTMLElement>('.project[data-project-id]')].map((el) => el.dataset.projectId ?? '') : null
      const id = jumpTargetId(slot, shown, orderedProjects(useStore.getState()))
      if (!id) return
      e.preventDefault()
      selectProject(id)
      // 選完才畫得出輸入框：等這一輪 render 完再給焦點，拿不到就退回專案標題（至少鍵盤位置對了）。
      requestAnimationFrame(() => {
        // 側欄捲到這個專案並讓它貼齊頂端（2026-09-16 使用者：「menu 也要 scroll 到指定點」）：
        // `nearest` 在它只露一半時什麼都不做，看起來像沒捲。
        const row = document.querySelector<HTMLElement>(`.project[data-project-id="${CSS.escape(id)}"]`)
        row?.scrollIntoView({ block: 'start', behavior: 'smooth' })
        const box = document.querySelector<HTMLTextAreaElement>('.composer textarea')
        if (box && !box.disabled) {
          // focus 會把元素捲進視野：`preventScroll` 保住上面那一捲，也避免主面板被拉動。
          box.focus({ preventScroll: true })
          return
        }
        document.querySelector<HTMLElement>('.project-label-btn.selected')?.focus({ preventScroll: true })
      })
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [selectProject])
}
