import { ApiError, type Run } from '../api/types.ts'
import { msg, t, tr } from '../lib/i18n'

export function visibleSuggestion(input: {
  kind: string | undefined
  run: Pick<Run, 'state' | 'agent_status' | 'prompt_suggestion'> | null | undefined
  draft: string
  attachments: number
  composerBusy: boolean
  draftBarOpen: boolean
}): string | null {
  const { kind, run } = input
  if (kind !== 'claude' || !run || run.state !== 'running' || run.agent_status !== 'idle') return null
  const text = run.prompt_suggestion?.trim()
  if (!text) return null
  if (input.draft.trim() !== '' || input.attachments > 0 || input.composerBusy || input.draftBarOpen) return null
  return text
}

export const SUGGESTION_REASON_TEXT: Record<string, string> = {
  suggestion_gone: msg('建議已經不在了（可能剛被用掉，或終端的輸入框裡有字），沒有送出'),
  suggestion_changed: msg('建議換成別句了，沒有送出；看一下新的再按'),
  tab_not_accepted: msg('終端沒有收下這句建議，沒有送出；到「終端」分頁看一下'),
  composer_unreadable: msg('讀不到終端的輸入框，沒有送出；到「終端」分頁看一下'),
  suggestion_unsupported: msg('這種 bot 沒有建議下一句'),
  'run mismatch': msg('這顆 bot 剛重啟過，建議已經過期，沒有送出'),
  'agent is busy': msg('Claude 正忙，這句建議已過期，沒有送出'),
}

export function suggestionFailureText(e: unknown, fallback: string): string {
  if (!(e instanceof ApiError) || e.status !== 409) return fallback
  const reason = typeof e.body.reason === 'string' ? e.body.reason : ''
  const base = SUGGESTION_REASON_TEXT[reason] ? tr(SUGGESTION_REASON_TEXT[reason]) : undefined
  if (!base) return fallback
  if (e.body.tab_sent === true && e.body.suggestion_restored === true) return t('{base}（已按 Tab，框裡那句也清掉了）', { base })
  if (e.body.tab_sent === true && e.body.suggestion_restored === false) return t('{base}（已按 Tab，那句還留在終端的輸入框裡）', { base })
  return base
}
