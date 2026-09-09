import { useLayoutEffect, useRef, type RefObject } from 'react'
import type { DraftKey } from '../store/store'
import { useStore } from '../store/store'

interface UseComposerFocusOptions {
  draftKey: DraftKey
  ref: RefObject<HTMLTextAreaElement | null>
  forceFocus?: boolean
  autoFocus?: boolean
}

/** Restore a draft selection without letting background state changes move focus. */
export function useComposerFocus({ draftKey, ref, forceFocus = false, autoFocus = true }: UseComposerFocusOptions) {
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    const currentText = useStore.getState().drafts[draftKey] ?? ''
    const saved = useStore.getState().draftCursors[draftKey]
    const max = currentText.length
    const start = Math.max(0, Math.min(max, saved?.start ?? max))
    const end = Math.max(start, Math.min(max, saved?.end ?? start))
    el.setSelectionRange(start, end)
  }, [draftKey, ref])

  const previousDraftKey = useRef<DraftKey | null>(null)
  const previousForceFocus = useRef(false)

  useLayoutEffect(() => {
    const firstMount = previousDraftKey.current === null
    const draftChanged = previousDraftKey.current !== draftKey
    const forceFocusRaised = forceFocus && !previousForceFocus.current
    previousDraftKey.current = draftKey
    previousForceFocus.current = forceFocus

    if ((!autoFocus || (!firstMount && !draftChanged)) && !forceFocusRaised) return
    const el = ref.current
    if (!el) return
    const composer = el.closest('.composer')
    const active = document.activeElement
    if (!composer || (active !== document.body && !composer.contains(active))) return
    el.focus()
  }, [autoFocus, draftKey, forceFocus, ref])
}
