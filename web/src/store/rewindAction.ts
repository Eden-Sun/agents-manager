import * as api from '../api'
import { ApiError } from '../api/types'
import { rewindErrText } from '../lib/rewind'
import { prependDraft } from './queuedSend'
import { useStore } from './store'
import { t } from '../lib/i18n'

export async function rewindAndRefill(
  botId: string,
  messageId: string,
  clearComposer?: string,
): Promise<boolean | { composerDraft: string }> {
  try {
    const r = await api.rewindBot(botId, messageId, clearComposer)
    const { drafts, setDraft, notify, loadMessages, messages } = useStore.getState()
    const key = `bot:${botId}` as const
    setDraft(key, prependDraft(r.text, drafts[key] ?? ''))
    const attachments = messages[botId]?.find((m) => m.id === messageId)?.attachments.length ?? 0
    const notice = r.paneCleared
      ? attachments > 0
        ? t('已倒回；原文放回輸入框了，可以改寫再送；{n} 個附件要重新加', { n: attachments })
        : t('已倒回；原文放回輸入框了，可以改寫再送')
      : attachments > 0
        ? t('已倒回；原文放回輸入框了。終端的輸入列裡還留著那段字，送下一則之前先到終端清掉；{n} 個附件要重新加', { n: attachments })
        : t('已倒回；原文放回輸入框了。終端的輸入列裡還留著那段字，送下一則之前先到終端清掉')
    notify(
      'info',
      notice,
    )
    void loadMessages(botId)
    return true
  } catch (e) {
    const draft = e instanceof ApiError && e.status === 409 && e.body.reason === 'composer_busy' ? e.body.draft : undefined
    if (typeof draft === 'string' && draft.trim()) return { composerDraft: draft }
    useStore.getState().notify('error', t('倒回失敗：{reason}', { reason: rewindErrText(e) }))
    return false
  }
}
