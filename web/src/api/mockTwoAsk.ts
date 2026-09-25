/**
 * mock 的「兩題 AskUserQuestion、pane 太矮」情境（`__amMock.twoAsk()`／prompt 含 `twoask`，issue #559）。
 *
 * 2026-09-25 使用者手機截圖：第一題單選兩項、第二題可複選兩項；終端太矮，分頁列沒畫出來（第二題的題目也沒有），
 * 只能從 `pending-question` 讀原題。這裡照 claude 的鍵位做一個小狀態機，讓畫面能一題一題答完、送出：
 * 單選頁 Enter 選定並跳下一題；複選頁 space 勾、走到 `Submit` 列按 Enter 進 review；review 頁 `1. Submit answers` 交卷。
 */

export interface TwoAskState {
  stage: 0 | 1 | 2
  /** 複選頁的 `Submit` 列是最後一格（`rows.length`）。 */
  cursor: number
  picked: number | null
  checked: boolean[]
}

export const TWO_ASK_QUESTIONS = [
  {
    question:
      '#253 預覽模式的三項實機驗收都過了（啟停釋放 pane / port、兩顆同時開、換版重啟後預覽原樣保留，Ctrl-C 掉 dev server 0.5 秒內轉 failed）。要關票嗎？',
    header: '#253 關票',
    multiSelect: false,
    options: [
      { label: '關票 (Recommended)', description: '驗收完成；下面兩個文字 / 排版小事若要改另開小票。' },
      { label: '先不關', description: '等我自己用過再說。' },
    ],
  },
  {
    question: '預覽面板的兩個小取捨要不要改？',
    header: '預覽 UI',
    multiSelect: true,
    options: [
      {
        label: '改啟動鍵文字',
        description: '同目錄已有 dev server 時，啟動鍵改成「接上 :5241」這類說法，不再收在「或由 AG Man 另起一顆」的摺疊裡。',
      },
      { label: '啟動預覽不收摺疊', description: '本機只有別的專案的 server 在跑時，「啟動預覽」直接顯示，不要收在摺疊裡。' },
    ],
  },
]

export const twoAskStart = (): TwoAskState => ({ stage: 0, cursor: 0, picked: null, checked: [false, false] })

/** 提示框上緣的分隔線還在，底下的分頁列與題目被裁掉。 */
const DIVIDER = '─'.repeat(40)
const mark = (on: boolean) => (on ? '❯' : ' ')

/** 單選頁多一列 `Type something.`，複選頁多一列 `[ ] Type something`（真機都有，這裡不接打字）。 */
const rowCount = (s: TwoAskState) => (s.stage === 0 ? 3 : s.stage === 1 ? 3 : 2)

export function twoAskScreen(s: TwoAskState): string[] {
  if (s.stage === 0) {
    const [a, b] = TWO_ASK_QUESTIONS[0].options
    // 使用者截圖那一格：分頁列被裁掉，第一題的題目還在（折成兩行）。
    const q = TWO_ASK_QUESTIONS[0].question
    return [
      DIVIDER,
      '',
      q.slice(0, 60),
      q.slice(60),
      '',
      `${mark(s.cursor === 0)} 1. ${a.label}`,
      `     ${a.description}`,
      `${mark(s.cursor === 1)} 2. ${b.label}`,
      `     ${b.description}`,
      `${mark(s.cursor === 2)} 3. Type something.`,
      DIVIDER,
      '  4. Chat about this',
      '',
      'Enter to select · Tab/Arrow keys to navigate · Esc to cancel',
    ]
  }
  if (s.stage === 1) {
    const [a, b] = TWO_ASK_QUESTIONS[1].options
    const box = (i: number) => (s.checked[i] ? '[✔]' : '[ ]')
    return [
      DIVIDER,
      `${mark(s.cursor === 0)} 1. ${box(0)} ${a.label}`,
      `       ${a.description}`,
      `${mark(s.cursor === 1)} 2. ${box(1)} ${b.label}`,
      `       ${b.description}`,
      `${mark(s.cursor === 2)} 3. [ ] Type something`,
      `${mark(s.cursor === 3)}    Submit`,
      DIVIDER,
      '  4. Chat about this',
      '',
      'Enter to select · ↑/↓ to navigate · Esc to cancel',
    ]
  }
  const q2 = TWO_ASK_QUESTIONS[1].options.filter((_, i) => s.checked[i]).map((o) => o.label)
  // pane 太矮：review 頁的分頁列一樣被裁掉。
  return [
    'Review your answers',
    '',
    ` ● ${TWO_ASK_QUESTIONS[0].question}`,
    `   → ${s.picked === null ? '' : TWO_ASK_QUESTIONS[0].options[s.picked]?.label ?? ''}`,
    ` ● ${TWO_ASK_QUESTIONS[1].question}`,
    `   → ${q2.join(', ')}`,
    '',
    'Ready to submit your answers?',
    '',
    `${mark(s.cursor === 0)} 1. Submit answers`,
    `${mark(s.cursor === 1)} 2. Cancel`,
  ]
}

/** 照順序套用按鍵；`done`＝交卷、`cancel`＝Esc／Cancel。 */
export function twoAskKeys(s: TwoAskState, keys: string[]): { state: TwoAskState; outcome: 'done' | 'cancel' | null } {
  let st: TwoAskState = { ...s, checked: [...s.checked] }
  const go = (stage: TwoAskState['stage']) => {
    st = { ...st, stage, cursor: 0 }
  }
  for (const k of keys) {
    const last = st.stage === 1 ? rowCount(st) : rowCount(st) - 1
    if (k === 'esc' || k === 'ctrl+c') return { state: st, outcome: 'cancel' }
    if (k === 'up') st.cursor = Math.max(0, st.cursor - 1)
    else if (k === 'down') st.cursor = Math.min(last, st.cursor + 1)
    else if (k === 'right' || k === 'tab') go(Math.min(2, st.stage + 1) as TwoAskState['stage'])
    else if (k === 'left' || k === 'shift+tab') go(Math.max(0, st.stage - 1) as TwoAskState['stage'])
    else if (k === 'space' && st.stage === 1 && st.cursor < 2) st.checked[st.cursor] = !st.checked[st.cursor]
    else if (k === 'enter') {
      if (st.stage === 0) {
        if (st.cursor < 2) {
          st.picked = st.cursor
          go(1)
        }
      } else if (st.stage === 1) {
        if (st.cursor === rowCount(st)) go(2)
        else if (st.cursor < 2) st.checked[st.cursor] = !st.checked[st.cursor]
      } else {
        return { state: st, outcome: st.cursor === 0 ? 'done' : 'cancel' }
      }
    }
  }
  return { state: st, outcome: null }
}
