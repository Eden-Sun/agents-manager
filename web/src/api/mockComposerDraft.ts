/**
 * mock 裡 bot 輸入框卡著的草稿（`__amMock.composerDraft('am-claude', '…')`）：照 daemon 回 409 `composer_busy`（帶 `draft`），
 * 並接住 `clear_draft`／`submit_draft`（`lifecycle/composer_draft.rs`）。截圖與手動看那一條用。
 */
import { ApiError } from './types'

type Rec = Record<string, unknown>

const SHOWN = 500

const shown = (d: string) => [...d].slice(0, SHOWN).join('')

function refuse(reason: string, draft: string | null): never {
  const fields = draft
    ? { draft: shown(draft), draft_truncated: [...draft].length > SHOWN, draft_actions: ['submit', 'clear'] }
    : { draft: null, draft_truncated: false, draft_actions: [] }
  throw new ApiError(409, { error: 'conflict', reason, retryable: reason !== 'draft_gone', sent: false, ...fields }, reason)
}

export class MockComposerDrafts {
  private drafts = new Map<string, string>()

  set(botId: string, text: string | null) {
    if (text) this.drafts.set(botId, text)
    else this.drafts.delete(botId)
  }

  /** 送 prompt 之前：回這一則的內容（`submit_draft` 時是框裡那段），框裡有字又沒帶動作就 409。 */
  gate(botId: string, b: Rec): string {
    const draft = this.drafts.get(botId) ?? null
    const expect = typeof b.expect_draft === 'string' ? b.expect_draft : null
    if (b.submit_draft === true) {
      if (!draft) refuse('draft_gone', null)
      if (shown(draft) !== expect) refuse('draft_changed', draft)
      this.drafts.delete(botId)
      return draft
    }
    if (draft) {
      if (b.clear_draft !== true) refuse('composer_busy', draft)
      if (shown(draft) !== expect) refuse('draft_changed', draft)
      this.drafts.delete(botId)
    }
    return String(b.text ?? '')
  }
}
