/**
 * 輸入框的 placeholder 與送出鍵的字（`ChatPanel` 的 `Composer`）。
 *
 * `composerState.queued` 有三種來源，字不能共用：回合還在跑（送出排到結束後）、已經有一則在等 bot 起來（排在它後面）、
 * bot 根本沒在跑（`autoStart`，issue #122：送出＝交給 daemon 先啟動再送）。第三種以前沿用第一種的字，
 * 對一顆停著的 bot 說「這回合還在跑」、按鈕寫「排隊送出」。
 */
export interface ComposerLabelState {
  disabled: boolean
  reason: string
  queued: boolean
  autoStart?: boolean
}

export function composerPlaceholder(state: ComposerLabelState, o: { phone: boolean; starting: boolean }): string {
  if (state.disabled) return `${state.reason || '目前無法送出訊息'}${o.phone ? '' : '——可以先打，恢復後再送'}`
  if (state.queued) {
    if (o.phone) return state.autoStart ? '輸入訊息…' : '下一則訊息…'
    if (state.autoStart) return '輸入訊息…（bot 沒在跑，送出會先啟動它）'
    return o.starting ? '先打下一則…（bot 起來、前一則送出後才輪到它）' : '這回合還在跑，先打下一則…（送出會排隊）'
  }
  return `輸入訊息…${o.phone ? '' : '（檔案可直接拖放或貼上）'}`
}

export function sendButtonLabel(state: ComposerLabelState, o: { phone: boolean; sending: boolean; uploading: boolean }): string {
  if (o.sending) return '送出中…'
  if (o.uploading) return '上傳中…'
  if (state.queued && state.autoStart) return o.phone ? '啟動送出' : '啟動並送出'
  if (state.queued) return o.phone ? '排隊' : '排隊送出'
  return '送出'
}

export function sendButtonTitle(state: ComposerLabelState, uploading: boolean): string | undefined {
  if (uploading) return '附件上傳中…'
  if (state.queued && state.autoStart) return '先啟動這顆 bot，起來後自動送出'
  return state.queued ? '這回合結束後自動送出' : undefined
}
