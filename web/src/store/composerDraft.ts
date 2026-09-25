/**
 * bot 的輸入框卡著一段沒送出的字（daemon 409 `composer_busy`）時，輸入列旁邊那一條（2026-09-26 w16T:p3）。
 *
 * 以前只跳一句「清掉或送出之後再送一次」的 toast，網頁上卻沒有地方能清掉或送出框裡那段，使用者只能自己去終端。
 * daemon 現在在 409 裡附上框裡的字（`draft`，截過）與這個 kind 驗過的動作（`draft_actions`）；這一條常駐到使用者
 * 選了動作或按取消，不會一閃就沒了。
 */
import { ApiError } from '../api/types.ts'

/** daemon 給的動作：`submit`＝對 pane 按 Enter 送出框裡那段；`clear`＝清掉它再送自己這則。 */
export type DraftAction = 'submit' | 'clear'

/** 帶著這個送 `POST /prompt`：`expect` 是 409 給的 `draft`，框裡換了字 daemon 會回 `draft_changed`、不動它。 */
export interface DraftRequest {
  action: DraftAction
  expect: string
}

export interface ComposerDraftBlock {
  /** 框裡那段（daemon 截過，最多 500 字）。 */
  draft: string
  truncated: boolean
  actions: DraftAction[]
  /** 正在做哪一個動作（按鈕先鎖住）。 */
  busy?: DraftAction
}

const ACTIONS: readonly DraftAction[] = ['submit', 'clear']

/** 409 裡描述草稿的欄位；舊 daemon 沒帶 `draft`（或讀不出字）＝`null`，照舊只跳通知。 */
export function draftBlockFrom(e: unknown): ComposerDraftBlock | null {
  if (!(e instanceof ApiError) || e.status !== 409) return null
  const reason = e.body.reason
  if (reason !== 'composer_busy' && reason !== 'draft_changed' && reason !== 'draft_uncleared') return null
  const draft = e.body.draft
  if (typeof draft !== 'string' || !draft.trim()) return null
  const raw = Array.isArray(e.body.draft_actions) ? e.body.draft_actions : []
  const actions = ACTIONS.filter((a) => raw.includes(a))
  return { draft, truncated: e.body.draft_truncated === true, actions }
}

/** 草稿動作失敗時的說明（沒列到的走一般錯誤文字）。 */
export const DRAFT_REASON_TEXT: Record<string, string> = {
  draft_changed: '框裡的字變了（可能有人正在終端打字），什麼都沒動；看一下新的內容再選一次',
  draft_uncleared: '沒清掉：按了清除鍵，框裡還是有字，所以沒有送出你這則；到「終端」分頁看一下',
  draft_gone: '框裡已經沒有字了，沒有東西可以送出',
  draft_clear_unsupported: '這種 bot 還沒驗過怎麼清輸入框，請到「終端」分頁處理',
  draft_clear_while_busy: '它正在跑一個回合，清輸入框會打斷它；等這一回合結束再清',
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
