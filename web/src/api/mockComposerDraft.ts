import { ApiError } from './types'

type Rec = Record<string, unknown>

const SHOWN = 500

const shown = (d: string) => [...d].slice(0, SHOWN).join('')

function draftToken(botId: string, draft: string): string {
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
  private busy = new Set<string>()

  setBusy(botId: string, on: boolean) {
    if (on) this.busy.add(botId)
    else this.busy.delete(botId)
  }

  has(botId: string): boolean {
    return this.drafts.has(botId)
  }

  snapshot(botId: string): Rec {
    const draft = this.drafts.get(botId)
    if (!draft) return { bot_id: botId, state: 'empty', busy: this.busy.has(botId), draft: null, draft_truncated: false, draft_token: null, draft_actions: [] }
    return { bot_id: botId, state: 'draft', busy: this.busy.has(botId), draft, draft_truncated: false, draft_token: draftToken(botId, draft), draft_actions: ['submit', 'clear'] }
  }

  set(botId: string, text: string | null) {
    if (text) this.drafts.set(botId, text)
    else this.drafts.delete(botId)
  }

  gateRewind(botId: string, b: Rec): void {
    const draft = this.drafts.get(botId)
    if (!draft) return
    const lf = (t: string) => t.replace(/\r\n/g, '\n')
    if (b.clear_composer !== true) {
      throw new ApiError(409, { error: 'conflict', reason: 'composer_busy', message: '終端輸入列裡有一段還沒送出的字，倒回前要先清掉它。', draft }, 'composer_busy')
    }
    if (typeof b.expect_composer !== 'string' || lf(b.expect_composer) !== lf(draft)) {
      throw new ApiError(409, { error: 'conflict', reason: 'composer_changed', message: '終端輸入列裡的字跟你看過的不一樣了，沒有動它。', draft }, 'composer_changed')
    }
    this.drafts.delete(botId)
  }

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
