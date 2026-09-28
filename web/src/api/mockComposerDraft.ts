/**
 * mock 裡 bot 輸入框卡著的草稿（`__amMock.composerDraft('am-claude', '…')`）：照 daemon 回 409 `composer_busy`（帶 `draft` 和 token），
 * 並接住 `clear_draft`／`submit_draft`（`lifecycle/composer_draft.rs`）。截圖與手動看那一條用。
 */
import { ApiError } from './types'

type Rec = Record<string, unknown>

const SHOWN = 500

const shown = (d: string) => [...d].slice(0, SHOWN).join('')

function draftToken(botId: string, draft: string): string {
  // Stable mock-only full-string fingerprint. The daemon uses SHA-256 bound to run and pane.
  let hash = 14695981039346656037n
  for (const byte of new TextEncoder().encode(`mock-composer-v1\0${botId}\0${draft}`)) {
    hash = BigInt.asUintN(64, (hash ^ BigInt(byte)) * 1099511628211n)
  }
  return `mock-${hash.toString(16).padStart(16, '0')}`
}

function refuse(reason: string, botId: string, draft: string | null): never {
  const fields = draft
    ? { draft: shown(draft), draft_truncated: [...draft].length > SHOWN, draft_token: draftToken(botId, draft), draft_actions: ['submit', 'clear'] }
    : { draft: null, draft_truncated: false, draft_token: null, draft_actions: [] }
  throw new ApiError(409, { error: 'conflict', reason, retryable: reason !== 'draft_gone', sent: false, ...fields }, reason)
}

export class MockComposerDrafts {
  private drafts = new Map<string, string>()

  /** 框裡有沒有字（#712：回合中切 fast 有草稿就不打，排到回合結束）。 */
  has(botId: string): boolean {
    return this.drafts.has(botId)
  }

  set(botId: string, text: string | null) {
    if (text) this.drafts.set(botId, text)
    else this.drafts.delete(botId)
  }

  /** 送 prompt 之前：回這一則的內容（`submit_draft` 時是框裡那段），框裡有字又沒帶動作就 409。 */
  gate(botId: string, b: Rec): string {
    const draft = this.drafts.get(botId) ?? null
    const expect = typeof b.expect_draft_token === 'string' ? b.expect_draft_token : null
    if (b.submit_draft === true) {
      if (!draft) refuse('draft_gone', botId, null)
      if (draftToken(botId, draft) !== expect) refuse('draft_changed', botId, draft)
      this.drafts.delete(botId)
      return draft
    }
    if (draft) {
      if (b.clear_draft !== true) refuse('composer_busy', botId, draft)
      if (draftToken(botId, draft) !== expect) refuse('draft_changed', botId, draft)
      this.drafts.delete(botId)
    }
    return String(b.text ?? '')
  }
}
