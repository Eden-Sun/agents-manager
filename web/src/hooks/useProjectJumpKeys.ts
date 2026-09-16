import { useEffect } from 'react'
import { orderedProjects, useStore } from '../store/store'

/** 1–9：`Digit1`…`Digit9`（看實體鍵位，不看輸入法或 Shift 打出什麼字）。 */
function slotOf(code: string): number | null {
  const m = /^Digit([1-9])$/.exec(code)
  return m ? Number(m[1]) - 1 : null
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
      const s = useStore.getState()
      const project = orderedProjects(s)[slot]
      if (!project) return
      e.preventDefault()
      selectProject(project.id)
      // 選完才畫得出輸入框：等這一輪 render 完再給焦點，拿不到就退回專案標題（至少鍵盤位置對了）。
      requestAnimationFrame(() => {
        const box = document.querySelector<HTMLTextAreaElement>('.composer textarea')
        if (box && !box.disabled) {
          box.focus()
          return
        }
        document.querySelector<HTMLElement>('.project-label-btn.selected')?.focus()
      })
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [selectProject])
}
