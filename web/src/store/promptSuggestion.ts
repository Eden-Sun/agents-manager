/**
 * claude 輸入框裡那句灰字「建議下一句」（終端 Tab 收下、Enter 送出）在網頁上的一鍵送出（2026-10-03 使用者）。
 * daemon 的 `POST /bots/:id/suggestion/accept` 對 pane 送的就是 Tab 與 Enter，跟終端一模一樣；這裡只管「什麼時候顯示」與「失敗講什麼」。
 */
import { ApiError, type Run } from '../api/types.ts'

/** 輸入列上方那一條要不要顯示：只有 claude 閒著、輸入框是空的、沒有別的東西占著輸入列才顯示。 */
export function visibleSuggestion(input: {
  kind: string | undefined
  run: Pick<Run, 'state' | 'agent_status' | 'prompt_suggestion'> | null | undefined
  /** 輸入框裡使用者打的字。 */
  draft: string
  attachments: number
  /** `composerState().disabled`／回合進行中／有排隊中的訊息。 */
  composerBusy: boolean
  /** 框裡卡著草稿（`composerDrafts`）的那一條開著。 */
  draftBarOpen: boolean
}): string | null {
  const { kind, run } = input
  if (kind !== 'claude' || !run || run.state !== 'running' || run.agent_status !== 'idle') return null
  const text = run.prompt_suggestion?.trim()
  if (!text) return null
  if (input.draft.trim() !== '' || input.attachments > 0 || input.composerBusy || input.draftBarOpen) return null
  return text
}

/** daemon 回 409 時的說明（沒列到的走一般錯誤文字）。 */
export const SUGGESTION_REASON_TEXT: Record<string, string> = {
  suggestion_gone: '建議已經不在了（可能剛被用掉，或終端的輸入框裡有字），沒有送出',
  suggestion_changed: '建議換成別句了，沒有送出；看一下新的再按',
  tab_not_accepted: '終端沒有收下這句建議，沒有送出；到「終端」分頁看一下',
  composer_unreadable: '讀不到終端的輸入框，沒有送出；到「終端」分頁看一下',
  suggestion_unsupported: '這種 bot 沒有建議下一句',
  'run mismatch': '這顆 bot 剛重啟過，建議已經過期，沒有送出',
  'agent is busy': 'Claude 正忙，這句建議已過期，沒有送出',
}

/** 把失敗講成人話；Tab 已經按了而且送不出去時，說清楚終端變成什麼樣。 */
export function suggestionFailureText(e: unknown, fallback: string): string {
  if (!(e instanceof ApiError) || e.status !== 409) return fallback
  const reason = typeof e.body.reason === 'string' ? e.body.reason : ''
  const base = SUGGESTION_REASON_TEXT[reason]
  if (!base) return fallback
  if (e.body.tab_sent === true && e.body.suggestion_restored === true) return `${base}（已按 Tab，框裡那句也清掉了）`
  if (e.body.tab_sent === true && e.body.suggestion_restored === false) return `${base}（已按 Tab，那句還留在終端的輸入框裡）`
  return base
}
