/**
 * 譯文快取只存文字：翻譯完成後才到的 Jev 建議、或同文字題目的 multiSelect 改變，畫面要立刻反映，且不再打翻譯。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { act, fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
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

const BASE: PendingQuestion = {
  header: 'Scope',
  question: "Which project's advisors should review which project's recent changes?",
  multiSelect: false,
  options: [
    { label: 'This project', description: 'Only the current project' },
    { label: 'All projects', description: '' },
  ],
  jev: null,
}
const SETTINGS = { enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto: true }] }
const isTranslate = (p: string) => p.endsWith('/pending-questions/translate')
const REPLY = {
  questions: [
    {
      id: 'q0',
      header: '範圍',
      question: '哪個專案的顧問，應該審哪個專案最近的改動？',
      options: [
        { id: 'q0-o0', label: '這個專案', description: '只看目前這個專案' },
        { id: 'q0-o1', label: '所有專案', description: '' },
      ],
    },
  ],
  model: 'claude-haiku-5-5',
  cached: false,
  cost_usd: null,
}

test('翻譯完成後才出現的 Jev 與 multiSelect 改變：立即顯示，且不再呼叫翻譯', async () => {
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS : isTranslate(r.path) ? REPLY : {}))
  let setQs: (qs: PendingQuestion[]) => void = () => {}
  function Harness() {
    const [qs, set] = useState<PendingQuestion[]>([BASE])
    setQs = set
    return <PendingQuestionCard questions={qs} defaultOpen answerWhere="below" />
  }
  const root = await mount(<Harness />)
  await until(() => root.querySelector('.pending-q')?.textContent?.includes('哪個專案的顧問') === true, '譯文出現')
  assert.equal(root.querySelector('.pending-q-jev'), null, '翻譯前沒有 Jev')
  assert.ok(!root.querySelector('.pending-q')?.textContent?.includes('可複選'))

  // 同一份文字，但 Jev 建議與複選後來才到。
  await act(async () => setQs([{ ...BASE, multiSelect: true, jev: { option: 1, label: 'All projects', score: 0.9 } }]))
  await until(() => root.querySelector('.pending-q')?.textContent?.includes('可複選') === true, '複選立即反映')
  assert.ok(root.querySelector('.pending-q-jev'), 'Jev 立即反映')
  assert.ok(root.querySelector('.pending-q')?.textContent?.includes('哪個專案的顧問'), '譯文仍在')
  await new Promise((r) => setTimeout(r, 30))
  assert.equal(reqs.filter((r) => isTranslate(r.path)).length, 1, '不再打翻譯')
})
