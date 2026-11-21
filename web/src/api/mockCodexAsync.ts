
export interface CodexAsyncState {
  expanded: boolean
  cursor: number
}

export const CODEX_ASYNC_OPTIONS = ['左側專案／bot 清單', 'bot 的「⋯」選單', '終端裡的選單']

export const CODEX_ASYNC_QUESTIONS = [
  {
    question: '你說的「智選 hub 的 menu 最下方」是哪個選單？',
    header: '',
    multiSelect: false,
    options: CODEX_ASYNC_OPTIONS.map((label) => ({ label, description: '' })),
    call_id: 'call_mock_async_1',
    index: 0,
  },
]

export const codexAsyncStart = (): CodexAsyncState => ({ expanded: false, cursor: 0 })

export function codexAsyncScreen(s: CodexAsyncState): string[] {
  const head = [
    '• 你說的「智選 hub 的 menu 最下方」是哪個選單？',
    '  • 左側專案／bot 清單',
    '  • bot 的「⋯」選單',
    '  • 終端裡的選單',
    '',
    '• Working (39s • esc to interrupt)',
    '',
    '• Queued follow-up inputs',
  ]
  if (!s.expanded) return [...head, '  ? 1 question · 7s', '    shift+← to answer', '', '› Ask Codex to do anything']
  const rows = [...CODEX_ASYNC_OPTIONS, 'Other']
  return [
    ...head,
    '  你說的「智選 hub 的 menu 最下方」是哪個選單？',
    ...rows.map((r, i) => `  ${i === s.cursor ? '›' : ' '} ${i + 1}. ${r}`),
    '  enter submit   ^] skip   shift+→ main prompt',
  ]
}

export function codexAsyncKeys(s: CodexAsyncState, keys: string[]): { state: CodexAsyncState; outcome: 'done' | null } {
  let state = s
  for (const k of keys) {
    if (!state.expanded) {
      if (k === 'shift+left') state = { ...state, expanded: true }
      continue
    }
    if (k === 'down') state = { ...state, cursor: Math.min(state.cursor + 1, CODEX_ASYNC_OPTIONS.length) }
    else if (k === 'up') state = { ...state, cursor: Math.max(state.cursor - 1, 0) }
    else if (k === 'shift+right') state = { ...state, expanded: false }
    else if (k === 'enter' && state.cursor < CODEX_ASYNC_OPTIONS.length) return { state, outcome: 'done' }
  }
  return { state, outcome: null }
}
