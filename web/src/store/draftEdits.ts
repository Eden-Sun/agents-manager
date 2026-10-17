import { useStore, type DraftKey } from './store'

/**
 * 送出期間記下這份草稿有沒有被改過（本機打字，或別的裝置經 WS 同步進來，都算）。送出成功回來時只有沒被改過才清框，
 * 否則會把送出途中才到的新草稿一起清掉，而且空字串還會同步回 daemon（#1811）。
 *
 * 看的是「改過沒」而不是「文字還一不一樣」：A → B → A 這種同文字但已經換過一輪的，也算改過。
 */
export function trackDraftEdits(key: DraftKey) {
  let edited = false
  const unsubscribe = useStore.subscribe((state, prev) => {
    if (state.drafts[key] !== prev.drafts[key]) edited = true
  })
  return {
    /** 送出期間草稿沒被改過，可以清。 */
    untouched: () => !edited,
    /** 送出回來就呼叫一次，把訂閱收掉。 */
    stop: unsubscribe,
  }
}
