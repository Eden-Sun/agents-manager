/**
 * Jev 的選項建議（issue #1475）：真的掛 `PendingQuestionCard` 與 `BlockedChoices` 進 happy-dom。
 * 有建議時該選項旁有「Jev 建議」＋分數；沒有建議、多選題什麼都沒有；只提示，點選項照舊走原本的鍵。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import { toPendingQuestions, pendingAtOnScreen } from '../lib/pendingQuestion'
import { parseChoiceMenu } from '../lib/tuiChoices'
import { BlockedChoices } from './BlockedChoices'
import { PendingQuestionCard } from './PendingQuestionCard'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const RAW = [
  {
    question: '連線方式？',
    header: '連線',
    multiSelect: false,
    options: [
      { label: '內網 http', description: '流量留在 VPC' },
      { label: '公網 https', description: '拿掉白名單' },
    ],
  },
]
const jev = (questions: unknown[]) => ({ ledger_id: 'L1', model: 'jev-1.13.0', threshold: 0.7, margin: 0.15, questions })
const CLEAR = jev([{ index: 0, scores: [0.91, 0.08], suggested: 0 }])
const CLOSE = jev([{ index: 0, scores: [0.85, 0.8], suggested: null }])

const tags = (root: HTMLElement) => [...root.querySelectorAll('.pending-q-jev, .pending-q-jev-line')].map((e) => e.textContent?.trim())

test('卡片：有建議時該選項旁標「Jev 建議」＋分數，其他選項沒有', async () => {
  const root = await mount(<PendingQuestionCard questions={toPendingQuestions(RAW, CLEAR)} defaultOpen answerWhere="below" />)
  const items = [...root.querySelectorAll('.pending-q-opts li')]
  assert.equal(items.length, 2)
  assert.match(items[0].textContent ?? '', /內網 http.*Jev 建議 0\.91/)
  assert.doesNotMatch(items[1].textContent ?? '', /Jev/)
  // 收合列也提一聲，不然預設收起時看不到。
  assert.ok(root.querySelector('.pending-q-toggle .pending-q-jev'))
})

test('卡片：沒有建議（沒開、分數太接近、多選題）什麼都不顯示', async () => {
  for (const [name, questions] of [
    ['沒有 jev_options', toPendingQuestions(RAW)],
    ['分數太接近', toPendingQuestions(RAW, CLOSE)],
    ['多選題', toPendingQuestions([{ ...RAW[0], multiSelect: true }], jev([{ index: 0, scores: [0.9, 0.1], suggested: 0 }]))],
  ] as const) {
    const root = await mount(<PendingQuestionCard questions={questions} defaultOpen answerWhere="below" />)
    assert.deepEqual(tags(root), [], name)
    assert.doesNotMatch(root.textContent ?? '', /Jev/, name)
    await unmountAll()
  }
})

test('卡片：選項在上面的選單裡時，建議改成一行說明（卡片不再列選項）', async () => {
  const root = await mount(<PendingQuestionCard questions={toPendingQuestions(RAW, CLEAR)} current={0} defaultOpen />)
  assert.equal(root.querySelectorAll('.pending-q-opts li').length, 0)
  assert.match(tags(root).join('|'), /Jev 建議 0\.91/)
  assert.match(root.querySelector('.pending-q-jev-line')?.textContent ?? '', /內網 http/)
})

// 2026-09-12 真機快照（bot `carbis`，185 欄）的選單，選項一～四。
const SCREEN = [
  ' ☐ 下一步',
  '',
  'robinstech-carbis-web 要先推進哪一塊？',
  '',
  '❯ 1. 先把 WIP commit 掉（建議）',
  '     57 個檔案未 commit。',
  '  2. 接 console 的回饋 API（#22）',
  '     把 api/feedback.ts 改成轉送 console。',
  '  3. 清小 issue',
  '     nginx server_name。',
  '  4. Type something.',
  '────────────────────────────────────────────────────────────',
  '  5. Chat about this',
  '',
  'Enter to select · ↑/↓ to navigate · Esc to cancel',
].join('\n')

const SCREEN_RAW = [
  {
    question: 'robinstech-carbis-web 要先推進哪一塊？',
    header: '下一步',
    options: [{ label: '先把 WIP commit 掉（建議）' }, { label: '接 console 的回饋 API（#22）' }, { label: '清小 issue' }],
  },
]

async function choices(questions: unknown) {
  const menu = parseChoiceMenu(SCREEN)
  assert.ok(menu, '選單要認得出來')
  const pending = toPendingQuestions(SCREEN_RAW, questions)
  const pendingAt = pendingAtOnScreen(pending, menu)
  assert.ok(pendingAt, '畫面上這題要對得上原題')
  return mount(<BlockedChoices botId="b1" menu={menu} pendingAt={pendingAt} />)
}

test('選單：有建議時對得上的那一列標「Jev 建議」＋分數，其他列沒有', async () => {
  const root = await choices(jev([{ index: 0, scores: [0.12, 0.88, 0.05], suggested: 1 }]))
  const rows = [...root.querySelectorAll('.bc-row')]
  const marked = rows.filter((r) => r.querySelector('.bc-jev'))
  assert.equal(marked.length, 1)
  assert.match(marked[0].querySelector('.bc-title')?.textContent ?? '', /接 console 的回饋 API/)
  assert.equal(marked[0].querySelector('.bc-jev')?.textContent?.trim(), 'Jev 建議 0.88')
})

test('選單：沒有建議、分數太接近時一列都不標', async () => {
  for (const q of [undefined, jev([{ index: 0, scores: [0.5, 0.45, 0.05], suggested: null }])]) {
    const root = await choices(q)
    assert.equal(root.querySelectorAll('.bc-jev').length, 0)
    assert.doesNotMatch(root.textContent ?? '', /Jev/)
    await unmountAll()
  }
})
