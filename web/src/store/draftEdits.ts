import { useStore, type DraftKey } from './store'

export function trackDraftEdits(key: DraftKey) {
  let edited = false
  const unsubscribe = useStore.subscribe((state, prev) => {
    if (state.drafts[key] !== prev.drafts[key]) edited = true
  })
  return {
    untouched: () => !edited,
    stop: unsubscribe,
  }
}
