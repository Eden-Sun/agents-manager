import * as api from '../api'
import { ApiError } from '../api/types'
import { rewindErrText } from '../lib/rewind'
import { prependDraft } from './queuedSend'
import { useStore } from './store'

/**
 * 倒回（SPEC §6.13，daemon 在終端驅動 claude 的 `/rewind`，不重啟）：成功就把那則的原文接回輸入框最前面（原本打到一半的字不丟），重拉這顆 bot 的訊息
 * （WS `messages_rewound` 也會標，這裡是斷線時的保險）。失敗只跳通知，輸入框不動。回是否成功。
 */
export async function rewindAndRefill(
  botId: string,
  messageId: string,
  clearComposer?: string,
): Promise<boolean | { composerDraft: string }> {
  try {
    const r = await api.rewindBot(botId, messageId, clearComposer)
    const { drafts, setDraft, notify, loadMessages } = useStore.getState()
    const key = `bot:${botId}` as const
    setDraft(key, prependDraft(r.text, drafts[key] ?? ''))
    notify(
      'info',
      r.paneCleared
        ? '已倒回；原文放回輸入框了，可以改寫再送'
        : '已倒回；原文放回輸入框了。終端的輸入列裡還留著那段字，送下一則之前先到終端清掉',
    )
    void loadMessages(botId)
    return true
  } catch (e) {
    // 終端輸入列有字（2026-10-01 使用者）：不只叫人去終端清，把那段字交給呼叫端，讓人看過再選「清掉再倒回」。
    const draft = e instanceof ApiError && e.status === 409 && e.body.reason === 'composer_busy' ? e.body.draft : undefined
    if (typeof draft === 'string' && draft.trim()) return { composerDraft: draft }
    useStore.getState().notify('error', `倒回失敗：${rewindErrText(e)}`)
    return false
  }
}
