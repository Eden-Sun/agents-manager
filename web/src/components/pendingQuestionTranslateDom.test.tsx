/**
 * 待答問卷（原題卡）的譯文：英文主體自動翻、可切換原文／譯文、失敗可手動重試；只換顯示，不送任何作答請求。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { PendingQuestion } from '../lib/pendingQuestion'
import { PendingQuestionCard } from './PendingQuestionCard'
import { resetTranslateReady } from './useTranslateReady'
import { resetPendingQuestionTranslation } from './usePendingQuestionTranslation'

before(setupDom)
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
    options: [
      { label: 'This project', description: 'Only the current project' },
      { label: 'All projects', description: '' },
    ],
  },
  {
    header: 'Window',
    question: 'How far back should "recent changes" go?',
    multiSelect: false,
    options: [{ label: 'Last 24 hours', description: 'since yesterday' }],
  },
]

const SETTINGS = (auto: boolean, enabled = true) => ({
  enabled: true,
  key_present: true,
  key_error: null,
  models: [],
  efforts: [],
  uses: [{ id: 'message_translate', enabled, auto }],
})

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
    {
      id: 'q1',
      header: '時間',
      question: '「最近的改動」要往回看多久？',
      options: [{ id: 'q1-o0', label: '最近 24 小時', description: '從昨天開始' }],
    },
  ],
  model: 'claude-haiku-5-5',
  cached: false,
  cost_usd: 0.0003,
}

const isTranslate = (p: string) => p.endsWith('/pending-questions/translate')
const text = () => document.querySelector('.pending-q')?.textContent ?? ''

test('英文問卷：auto 開著就自動翻一次，顯示譯文；送出的只有翻譯請求，沒有作答', async () => {
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS(true) : isTranslate(r.path) ? REPLY : {}))
  await mount(<PendingQuestionCard questions={QS} defaultOpen answerWhere="below" />)
  await until(() => text().includes('哪個專案的顧問'), '自動翻出譯文')
  assert.equal(reqs.filter((r) => isTranslate(r.path)).length, 1, '整份問卷只打一次')
  const body = reqs.find((r) => isTranslate(r.path))?.body as { questions: { options: unknown[] }[] }
  assert.equal(body.questions.length, 2, '兩題一起送')
  assert.match(text(), /這個專案/)
  assert.match(text(), /最近 24 小時/)
  assert.doesNotMatch(text(), /Which project/)
  assert.equal(document.querySelectorAll('.pending-q-opts li').length, 3, '選項數不變')
  assert.ok(!reqs.some((r) => r.method === 'POST' && !isTranslate(r.path)), '沒有任何作答或按鍵請求')
})

test('切換原文／譯文；切回原文後不再自動翻回去，再點才譯', async () => {
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS(true) : isTranslate(r.path) ? REPLY : {}))
  await mount(<PendingQuestionCard questions={QS} defaultOpen answerWhere="below" />)
  await until(() => text().includes('哪個專案的顧問'), '譯文')
  const btn = () => document.querySelector('.pending-q-tr .msg-translate') as HTMLButtonElement
  assert.equal(btn().textContent, '⇄ 原文')
  await click(btn())
  assert.match(text(), /Which project/)
  assert.equal(btn().textContent, '⇄ 譯文')
  await unmountAll()
  await mount(<PendingQuestionCard questions={QS} defaultOpen answerWhere="below" />)
  await until(() => btn() !== null, '重新掛載')
  await new Promise((r) => setTimeout(r, 30))
  assert.match(text(), /Which project/, '手動點回原文後，重新掛載不自動翻回去')
  assert.equal(reqs.filter((r) => isTranslate(r.path)).length, 1, '重新掛載不重打')
  await click(btn())
  await until(() => text().includes('哪個專案的顧問'), '再點才譯')
})

test('auto 關著：不自動翻，按「翻譯」才打', async () => {
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS(false) : isTranslate(r.path) ? REPLY : {}))
  await mount(<PendingQuestionCard questions={QS} defaultOpen answerWhere="below" />)
  await until(() => document.querySelector('.pending-q-tr .msg-translate') !== null, '翻譯鈕')
  assert.equal(document.querySelector('.pending-q-tr .msg-translate')?.textContent, '翻譯')
  assert.equal(reqs.filter((r) => isTranslate(r.path)).length, 0)
  await click(document.querySelector('.pending-q-tr .msg-translate')!)
  await until(() => text().includes('哪個專案的顧問'), '手動譯')
})

test('未啟用：按鈕標「翻譯（未啟用）」，不打翻譯 API', async () => {
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS(true, false) : {}))
  await mount(<PendingQuestionCard questions={QS} defaultOpen answerWhere="below" />)
  await until(() => document.querySelector('.msg-translate.off') !== null, '未啟用')
  assert.equal(document.querySelector('.msg-translate')?.textContent, '翻譯（未啟用）')
  await click(document.querySelector('.msg-translate')!)
  assert.ok(!reqs.some((r) => isTranslate(r.path)))
})

test('失敗：自動翻失敗不重試，手動「翻譯」可重試成功', async () => {
  let fail = true
  const reqs = fakeApi((r) => {
    if (r.path.endsWith('/claude-api/settings')) return SETTINGS(true)
    if (isTranslate(r.path)) {
      if (fail) throw new Error('boom')
      return REPLY
    }
    return {}
  })
  await mount(<PendingQuestionCard questions={QS} defaultOpen answerWhere="below" />)
  await until(() => reqs.some((r) => isTranslate(r.path)), '自動試一次')
  await new Promise((r) => setTimeout(r, 30))
  assert.equal(reqs.filter((r) => isTranslate(r.path)).length, 1, '自動翻失敗不一直重試')
  assert.match(text(), /Which project/, '失敗停在原文')
  fail = false
  await click(document.querySelector('.pending-q-tr .msg-translate')!)
  await until(() => text().includes('哪個專案的顧問'), '手動重試成功')
})

test('中文問卷：沒有翻譯鈕，也不打 API', async () => {
  const zh: PendingQuestion[] = [{ header: '範圍', question: '要審哪個專案的改動？', multiSelect: false, options: [{ label: '這個專案', description: '' }] }]
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS(true) : {}))
  await mount(<PendingQuestionCard questions={zh} defaultOpen answerWhere="below" />)
  await new Promise((r) => setTimeout(r, 30))
  assert.equal(document.querySelector('.pending-q-tr'), null)
  assert.ok(!reqs.some((r) => isTranslate(r.path)))
})
