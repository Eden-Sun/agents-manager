
export interface TwoAskState {
  stage: 0 | 1 | 2
  cursor: number
  picked: number | null
  checked: boolean[]
  questions?: typeof TWO_ASK_QUESTIONS
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

export const twoAskStart = (questions?: typeof TWO_ASK_QUESTIONS): TwoAskState => ({ stage: 0, cursor: 0, picked: null, checked: [false, false], ...(questions ? { questions } : {}) })

const DIVIDER = '─'.repeat(40)
const mark = (on: boolean) => (on ? '❯' : ' ')

const rowCount = (s: TwoAskState) => (s.stage === 0 ? 3 : s.stage === 1 ? 3 : 2)

export function twoAskScreen(s: TwoAskState): string[] {
  const QS = s.questions ?? TWO_ASK_QUESTIONS
  if (s.stage === 0) {
    const [a, b] = QS[0].options
    const q = QS[0].question
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
    const [a, b] = QS[1].options
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
  const q2 = QS[1].options.filter((_, i) => s.checked[i]).map((o) => o.label)
  return [
    'Review your answers',
    '',
    ` ● ${QS[0].question}`,
    `   → ${s.picked === null ? '' : QS[0].options[s.picked]?.label ?? ''}`,
    ` ● ${QS[1].question}`,
    `   → ${q2.join(', ')}`,
    '',
    'Ready to submit your answers?',
    '',
    `${mark(s.cursor === 0)} 1. Submit answers`,
    `${mark(s.cursor === 1)} 2. Cancel`,
  ]
}

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

export const SURVEY_EN_PAGES = [
  { label: 'Who', q: 'Who writes the articles?', opts: ['Non-engineers publish alone', 'Engineers edit Markdown'] },
  { label: 'Features', q: 'Which features do you need?', opts: ['Site search, categories, tags', 'Newsletter subscription'] },
]

const SURVEY_EN_BAR = '←  ☒ Who  ☒ Features  ✔ Submit  →'

export function surveyEnScreen(tab: number): string[] {
  if (tab >= 2) return surveyEnReviewScreen()
  const p = SURVEY_EN_PAGES[tab]
  return ['─'.repeat(60), SURVEY_EN_BAR, '', p.q, '', ...p.opts.map((o, i) => `${i === 0 ? '❯ ' : '  '}${i + 1}. ${o}`), '', 'Enter to select · Tab/Arrow keys to navigate · Esc to cancel']
}

export function surveyEnReviewScreen(): string[] {
  return [
    '─'.repeat(60),
    SURVEY_EN_BAR,
    '',
    'Review your answers',
    '',
    ' ● Who writes the articles?',
    '   → Engineers edit Markdown',
    ' ● Which features do you need?',
    '   → Newsletter subscription',
    '',
    'Ready to submit your answers?',
    '',
    '❯ 1. Submit answers',
    '  2. Cancel',
    '',
    'Enter to select · Tab/Arrow keys to navigate · Esc to cancel',
  ]
}

export function mockTranslateEcho(body: unknown) {
  const qs = ((body as { questions?: { header: string; question: string; options: { label: string; description: string }[] }[] }).questions ?? [])
  return {
    questions: qs.map((q, qi) => ({
      id: `q${qi}`,
      header: q.header ? `〔譯〕${q.header}` : '',
      question: `〔譯〕${q.question}`,
      options: q.options.map((o, oi) => ({ id: `q${qi}-o${oi}`, label: `〔譯〕${o.label}`, description: o.description ? `〔譯〕${o.description}` : '' })),
    })),
    model: 'claude-haiku-5-5',
    cached: false,
    cost_usd: null,
  }
}
