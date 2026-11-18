/**
 * bot 的輸入框卡著一段沒送出的字（daemon 409 `composer_busy`）時，輸入列旁邊那一條（2026-09-26 w16T:p3）。
 *
 * 以前只跳一句「清掉或送出之後再送一次」的 toast，網頁上卻沒有地方能清掉或送出框裡那段，使用者只能自己去終端。
 * daemon 現在在 409 裡附上框裡的字（`draft`，截過）、完整草稿識別碼（`draft_token`）與這個 kind 驗過的動作（`draft_actions`）；這一條常駐到使用者
 * 選了動作或按取消，不會一閃就沒了。
 */
import { ApiError } from '../api/types.ts'
import { msg } from '../lib/i18n'

/** daemon 給的動作：`submit`＝對 pane 按 Enter 送出框裡那段；`clear`＝清掉它再送自己這則。 */
export type DraftAction = 'submit' | 'clear'

/** 帶著這個送 `POST /prompt`：`token` 是 409 給的完整草稿識別碼，框裡換了字 daemon 會回 `draft_changed`、不動它。 */
export interface DraftRequest {
  action: DraftAction
  token: string
}

export interface ComposerDraftBlock {
  /** 框裡那段（daemon 截過，最多 500 字）。 */
  draft: string
  /** 完整草稿與 run/pane 的識別碼；不可用顯示用的 `draft` 代替。 */
  token: string
  truncated: boolean
  actions: DraftAction[]
  /** 正在做哪一個動作（按鈕先鎖住）。 */
  busy?: DraftAction
  /** 使用者按了「編輯後送出」：這段已放進輸入框，下一次送出走 `clear_draft`＋這個 token（改過的 `text`）。 */
  editing?: boolean
  /** 這一條是哪個 run 的畫面：run 換了（重啟）它就不再成立，自動收掉（`draftIsStale`）。 */
  runId?: string
  /**
   * 畫面不是輸入框、是已辨識的擋路對話框（codex hook 審核框等）：這句是 daemon 給的說明。沒有草稿、沒有任何草稿動作——
   * 對話框的游標列不是草稿，「送出框裡那段」等於替使用者在對話框上按 Enter（#1989）。
   */
  dialog?: string
  /** 一出現就自動進編輯（Session paused 選了「Edit prompt and retry」）；放進輸入框後由輸入列清掉。 */
  editNow?: boolean
}

const ACTIONS: readonly DraftAction[] = ['submit', 'clear']

/** 409 裡描述草稿的欄位；讀不到內容時仍保留說明，但不能提供草稿動作。 */
export function draftBlockFrom(e: unknown, currentRunId?: string): ComposerDraftBlock | null {
  const block = draftBlockOf(e)
  if (!block) return null
  const runId = e instanceof ApiError && typeof e.body.run_id === 'string' && e.body.run_id ? e.body.run_id : currentRunId
  return runId ? { ...block, runId } : block
}

function draftBlockOf(e: unknown): ComposerDraftBlock | null {
  if (!(e instanceof ApiError) || e.status !== 409) return null
  const reason = e.body.reason
  // 草稿動作撞上對話框（daemon 一個鍵都沒按）：那一條換成對話框說明，不再給草稿動作。一般送出的 `dialog_open` 沒有 `dialog` 欄位，不走這裡。
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

/** 從 `GET /composer-draft` 的結果做一條（`state` 不是有字、或沒有可用的動作就回 `null`）。`dialog`＝只有說明、沒有任何草稿動作。 */
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

/** 這一條是別的 run 的畫面（bot 重啟過、或 run 已經沒了）：不再成立，要收掉。沒記 run 的舊列不判。 */
export function draftIsStale(block: ComposerDraftBlock | undefined, currentRunId: string | undefined): boolean {
  return Boolean(block?.runId) && block?.runId !== currentRunId
}

/** run 表變了之後還成立的草稿列；沒有變動回原物件（不觸發重繪）。 */
export function pruneStaleDrafts(
  drafts: Record<string, ComposerDraftBlock>,
  runs: Record<string, { id: string } | null | undefined>,
): Record<string, ComposerDraftBlock> {
  const stale = Object.keys(drafts).filter((botId) => draftIsStale(drafts[botId], runs[botId]?.id))
  if (stale.length === 0) return drafts
  return Object.fromEntries(Object.entries(drafts).filter(([botId]) => !stale.includes(botId)))
}

/** 草稿動作失敗時的說明（沒列到的走一般錯誤文字）。 */
export const DRAFT_REASON_TEXT: Record<string, string> = {
  draft_changed: msg('框裡的字變了（可能有人正在終端打字），什麼都沒動；看一下新的內容再選一次'),
  draft_uncleared: msg('沒清掉：按了清除鍵，框裡還是有字，所以沒有送出你這則；到「終端」分頁看一下'),
  draft_gone: msg('框裡已經沒有字了，沒有東西可以送出'),
  draft_clear_unsupported: msg('這種 bot 還沒驗過怎麼清輸入框，請到「終端」分頁處理'),
  draft_clear_while_busy: msg('它正在跑一個回合，清輸入框會打斷它；等這一回合結束再清'),
}

/** 按下動作到 daemon 回話之間鎖住那一條的按鈕（`undefined`＝解鎖）。沒有那一條就不動。 */
export function markDraftBusy(
  s: { composerDrafts: Record<string, ComposerDraftBlock> },
  botId: string,
  busy: DraftAction | undefined,
): { composerDrafts?: Record<string, ComposerDraftBlock> } {
  const cur = s.composerDrafts[botId]
  if (!cur) return {}
  return { composerDrafts: { ...s.composerDrafts, [botId]: { ...cur, busy } } }
}

/** 草稿動作之後 daemon 說框裡已經沒有字：那一條沒有東西可以處理了，收掉。 */
export function draftIsGone(e: unknown): boolean {
  return e instanceof ApiError && e.status === 409 && e.body.reason === 'draft_gone'
}
