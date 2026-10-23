import { msg, t, tr } from './i18n'

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
  if (state.disabled) return o.phone
    ? state.reason || t('目前無法送出訊息')
    : t('{reason}——可以先打，恢復後再送', { reason: state.reason || t('目前無法送出訊息') })
  if (state.queued) {
    if (o.phone) return state.autoStart ? t('輸入訊息…') : t('下一則訊息…')
    if (state.autoStart) return t('輸入訊息…（bot 沒在跑，送出會先啟動它）')
    return o.starting
      ? t('先打下一則…（bot 起來、前一則送出後才輪到它）')
      : t('這回合還在跑，先打下一則…（送出會排隊）')
  }
  return o.phone ? t('輸入訊息…') : t('輸入訊息…（檔案可直接拖放或貼上）')
}

/** 有附件上傳失敗時按送出（或 Enter）的提示；送出會少了那個檔案（issue #917）。 */
export const ATTACHMENT_FAILED_NOTICE = msg('有附件上傳失敗：按卡片上的「重試」或把它移除，再送')

export function sendButtonLabel(state: ComposerLabelState, o: { phone: boolean; sending: boolean; uploading: boolean; failed?: boolean }): string {
  if (o.sending) return t('送出中…')
  if (o.uploading) return t('上傳中…')
  if (o.failed) return t('有附件上傳失敗')
  if (state.queued && state.autoStart) return o.phone ? t('啟動送出') : t('啟動並送出')
  if (state.queued) return o.phone ? t('排隊') : t('排隊送出')
  return t('送出')
}

export function sendButtonTitle(state: ComposerLabelState, uploading: boolean, failed = false): string | undefined {
  if (uploading) return t('附件上傳中…')
  if (failed) return tr(ATTACHMENT_FAILED_NOTICE)
  if (state.queued && state.autoStart) return t('先啟動這顆 bot，起來後自動送出')
  return state.queued ? t('這回合結束後自動送出') : undefined
}
