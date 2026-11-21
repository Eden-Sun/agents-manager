import { useEffect } from 'react'
import { dialogOpen } from '../lib/dialogOpen'
import { isImeEnter } from '../lib/ime'
import { scrollProjectIntoView } from '../lib/scrollProject'
import { settingsBlocksLeave } from '../lib/settingsLeaveGuard'
import { orderedProjects, useStore } from '../store/store'

function slotOf(code: string): number | null {
  const m = /^Digit([1-9])$/.exec(code)
  return m ? Number(m[1]) - 1 : null
}

export function jumpTargetId(slot: number, shown: readonly string[] | null, ordered: readonly { id: string }[]): string | null {
  return (shown ? shown[slot] : ordered[slot]?.id) ?? null
}

export function useProjectJumpKeys() {
  const selectProject = useStore((s) => s.selectProject)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.ctrlKey || e.metaKey || e.altKey || e.shiftKey || isImeEnter(e) || e.defaultPrevented) return
      const slot = slotOf(e.code)
      if (slot === null) return
      if (dialogOpen()) return
      if (settingsBlocksLeave()) {
        e.preventDefault()
        return
      }
      const nav = document.querySelector('.sidebar-scroll')
      const shown = nav ? [...nav.querySelectorAll<HTMLElement>('.project[data-project-id]')].map((el) => el.dataset.projectId ?? '') : null
      const id = jumpTargetId(slot, shown, orderedProjects(useStore.getState()))
      if (!id) return
      e.preventDefault()
      selectProject(id)
      requestAnimationFrame(() => {
        scrollProjectIntoView(id)
        document.querySelector<HTMLElement>('.project-label-btn.selected')?.focus({ preventScroll: true })
      })
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [selectProject])
}
