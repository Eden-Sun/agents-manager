import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { trackDraftEdits } from '../store/draftEdits'
import './composerDraftBar.css'

/**
 * bot 的輸入框卡著一段沒送出的字（409 `composer_busy`）：顯示框裡那段，給三個動作（2026-09-26 w16T:p3）。
 * 「送出框裡那段」＝daemon 對 pane 按 Enter、照一般 prompt 開回合；「清掉再送我這則」＝daemon 清框、確認框空了才打字；
 * 「取消」只收掉這一條，框裡的字不動。daemon 沒給的動作（沒驗過的 kind、讀不出字）不顯示。
 */
export function ComposerDraftBar({
  botId,
  text,
  attachments,
  attachmentsBlocked,
  onSent,
}: {
  botId: string
  /** 輸入框裡使用者自己要送的字（清掉再送的就是它）。 */
  text: string
  attachments: string[]
  attachmentsBlocked?: 'uploading' | 'failed' | null
  /** 自己這則送出去了：清附件；`textUntouched` ＝ 送出途中輸入框的字沒被改過，才可以清字（#1811）。 */
  onSent: (textUntouched: boolean) => void
}) {
  const block = useStore((s) => s.composerDrafts[botId])
  const sendPrompt = useStore((s) => s.sendPrompt)
  const dismiss = useStore((s) => s.dismissComposerDraft)
  const t = useT()
  if (!block) return null
  const busy = Boolean(block.busy)
  const mine = text.trim()
  const hasMine = mine !== '' || attachments.length > 0

  if (!block.draft) {
    return (
      <div className="composer-draft" role="status">
        <span className="composer-draft-label">{t('讀不到終端內容，請到終端檢查；目前訊息保留')}</span>
        <div className="composer-draft-actions">
          <button type="button" className="mini-btn" onClick={() => dismiss(botId)}>{t('取消')}</button>
        </div>
      </div>
    )
  }

  const submitDraft = () => {
    void sendPrompt(botId, '', [], false, false, { action: 'submit', token: block.token })
  }
  const clearAndSend = () => {
    if (attachmentsBlocked) return
    const edits = trackDraftEdits(`bot:${botId}`)
    void sendPrompt(botId, mine, attachments, false, false, { action: 'clear', token: block.token }).then((ok) => {
      if (ok) onSent(edits.untouched())
    }).finally(edits.stop)
  }

  return (
    <div className="composer-draft" role="status">
      <span className="composer-draft-label">{t('沒送出：bot 的輸入框裡還有一段沒送出的字')}</span>
      <pre className="composer-draft-text">
        {block.draft}
        {block.truncated ? '…' : ''}
      </pre>
      {block.truncated ? <span className="composer-draft-cut">{t('只顯示前 500 字')}</span> : null}
      {block.actions.length === 0 ? <span>{t('這顆 bot 不能從網頁處理框裡的字，請到「終端」分頁清掉或送出。')}</span> : null}
      <div className="composer-draft-actions">
        {block.actions.includes('submit') ? (
          <button type="button" className="mini-btn primary" disabled={busy} title={t("在終端按 Enter，把框裡這段當成一則訊息送出")} onClick={submitDraft}>
            {block.busy === 'submit' ? t('送出中…') : t('送出框裡那段')}
          </button>
        ) : null}
        {block.actions.includes('clear') ? (
          <button
            type="button"
            className="mini-btn"
            disabled={busy || !hasMine || Boolean(attachmentsBlocked)}
            title={
              attachmentsBlocked === 'uploading'
                ? t('附件還在上傳，傳完再送')
                : attachmentsBlocked === 'failed'
                  ? t('有附件上傳失敗：先重試或移除')
                  : hasMine
                    ? t('清掉框裡這段（確認框空了才打字），再送出你輸入的這則')
                    : t('輸入框裡還沒有你要送的字')
            }
            onClick={clearAndSend}
          >
            {block.busy === 'clear' ? t('清除中…') : t('清掉再送我這則')}
          </button>
        ) : null}
        <button type="button" className="mini-btn" disabled={busy} title={t("收起這一條，框裡的字不動")} onClick={() => dismiss(botId)}>
          {t('取消')}
        </button>
      </div>
    </div>
  )
}
