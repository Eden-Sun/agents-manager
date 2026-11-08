/**
 * 同一份待答問卷同時掛上兩個元件（例如面板與視窗）：自動翻譯只能打一次，不能各自因為先前算好的 `fresh` 再起一份。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { PendingQuestion } from '../lib/pendingQuestion'
import { PendingQuestionCard } from './PendingQuestionCard'
import { resetTranslateReady } from './useTranslateReady'
import { resetPendingQuestionTranslation } from './usePendingQuestionTranslation'

before(setupDom)
beforeEach(() => {
  resetTranslateReady()
  resetPendingQuestionTranslation()
})
afterEach(async () => {
  await unmountAll()
  resetTranslateReady()
  resetPendingQuestionTranslation()
})
after(teardownDom)

const QS: PendingQuestion[] = [
  {
    header: 'Scope',
    question: "Which project's advisors should review which project's recent changes?",
    multiSelect: false,
    options: [{ label: 'This project', description: 'Only the current project' }],
    jev: null,
  },
]
const SETTINGS = { enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto: true }] }
const isTranslate = (p: string) => p.endsWith('/pending-questions/translate')
const REPLY = {
  questions: [{ id: 'q0', header: '範圍', question: '哪個專案的顧問該審？', options: [{ id: 'q0-o0', label: '這個專案', description: '只看這個' }] }],
  model: 'claude-haiku-5-5',
  cached: false,
  cost_usd: null,
}

test('兩個元件同時掛上同一份問卷：自動翻譯只打一次，兩邊都顯示譯文', async () => {
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS : isTranslate(r.path) ? REPLY : {}))
  const root = await mount(
    <div>
      <PendingQuestionCard questions={QS} defaultOpen answerWhere="below" />
      <PendingQuestionCard questions={QS} defaultOpen answerWhere="below" />
    </div>,
  )
  await until(() => root.querySelectorAll('.pending-q')[1]?.textContent?.includes('哪個專案的顧問') === true, '兩邊都顯示譯文')
  await new Promise((r) => setTimeout(r, 30))
  assert.equal(reqs.filter((r) => isTranslate(r.path)).length, 1, '整份問卷只打一次')
  assert.ok(root.querySelectorAll('.pending-q')[0]?.textContent?.includes('哪個專案的顧問'))
})
