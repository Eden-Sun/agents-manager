import { ApiError } from '../api/types.ts'
import { msg } from '../lib/i18n'

export type DraftAction = 'submit' | 'clear'

export interface DraftRequest {
  action: DraftAction
  token: string
}

export interface ComposerDraftBlock {
  draft: string
  token: string
  truncated: boolean
  actions: DraftAction[]
  busy?: DraftAction
  editing?: boolean
  runId?: string
  dialog?: string
  editNow?: boolean
}

const ACTIONS: readonly DraftAction[] = ['submit', 'clear']

export function draftBlockFrom(e: unknown, currentRunId?: string): ComposerDraftBlock | null {
  const block = draftBlockOf(e)
  if (!block) return null
  const runId = e instanceof ApiError && typeof e.body.run_id === 'string' && e.body.run_id ? e.body.run_id : currentRunId
  return runId ? { ...block, runId } : block
}

function draftBlockOf(e: unknown): ComposerDraftBlock | null {
  if (!(e instanceof ApiError) || e.status !== 409) return null
  const reason = e.body.reason
  if (reason === 'dialog_open' && typeof e.body.dialog === 'string') {
    return { draft: '', token: '', truncated: false, actions: [], dialog: typeof e.body.message === 'string' && e.body.message ? e.body.message : msg('bot 停在一個對話框，請到「終端」處理或重新啟動這顆 bot') }
  }
  if (reason !== 'composer_busy' && reason !== 'draft_changed' && reason !== 'draft_uncleared') return null
  const draft = e.body.draft
  if (typeof draft !== 'string' || !draft.trim()) {
    return reason === 'composer_busy' ? { draft: '', token: '', truncated: false, actions: [] } : null
  }
  const raw = Array.isArray(e.body.draft_actions) ? e.body.draft_actions : []
  const actions = ACTIONS.filter((a) => raw.includes(a))
  const token = typeof e.body.draft_token === 'string' ? e.body.draft_token : ''
  return { draft, token, truncated: e.body.draft_truncated === true, actions: token ? actions : [] }
}

export function draftBlockFromSnapshot(
  snap: { state: string; draft: string; token: string; actions: DraftAction[]; runId?: string; dialog?: string },
  editNow = false,
): ComposerDraftBlock | null {
  const run = snap.runId ? { runId: snap.runId } : {}
  if (snap.state === 'dialog') {
    return { draft: '', token: '', truncated: false, actions: [], dialog: snap.dialog || msg('bot 停在一個對話框，請到「終端」處理或重新啟動這顆 bot'), ...run }
  }
  if (snap.state !== 'draft' || !snap.draft.trim()) return null
  return { draft: snap.draft, token: snap.token, truncated: false, actions: snap.token ? snap.actions : [], ...run, ...(editNow ? { editNow: true } : {}) }
}

export function draftIsStale(block: ComposerDraftBlock | undefined, currentRunId: string | undefined): boolean {
  return Boolean(block?.runId) && block?.runId !== currentRunId
}

export function pruneStaleDrafts(
  drafts: Record<string, ComposerDraftBlock>,
  runs: Record<string, { id: string } | null | undefined>,
): Record<string, ComposerDraftBlock> {
  const stale = Object.keys(drafts).filter((botId) => draftIsStale(drafts[botId], runs[botId]?.id))
  if (stale.length === 0) return drafts
  return Object.fromEntries(Object.entries(drafts).filter(([botId]) => !stale.includes(botId)))
}

export const DRAFT_REASON_TEXT: Record<string, string> = {
  draft_changed: msg('框裡的字變了（可能有人正在終端打字），什麼都沒動；看一下新的內容再選一次'),
  draft_uncleared: msg('沒清掉：按了清除鍵，框裡還是有字，所以沒有送出你這則；到「終端」分頁看一下'),
  draft_gone: msg('框裡已經沒有字了，沒有東西可以送出'),
  draft_clear_unsupported: msg('這種 bot 還沒驗過怎麼清輸入框，請到「終端」分頁處理'),
  draft_clear_while_busy: msg('它正在跑一個回合，清輸入框會打斷它；等這一回合結束再清'),
}

export function markDraftBusy(
  s: { composerDrafts: Record<string, ComposerDraftBlock> },
  botId: string,
  busy: DraftAction | undefined,
): { composerDrafts?: Record<string, ComposerDraftBlock> } {
  const cur = s.composerDrafts[botId]
  if (!cur) return {}
  return { composerDrafts: { ...s.composerDrafts, [botId]: { ...cur, busy } } }
}

export function draftIsGone(e: unknown): boolean {
  return e instanceof ApiError && e.status === 409 && e.body.reason === 'draft_gone'
}
