import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  jevMatchesTitle,
  pendingAtOnScreen,
  pendingIndexOnScreen,
  questionVisibleOnScreen,
  toPendingQuestions,
  toPendingQuestionsFromResponse,
} from './pendingQuestion'

const RAW = [
  {
    question: 'prod console 連 prod RPA 要走哪條路？（現在失敗是因為 console 規定 RPA 位址必須 https）',
    header: '連線方式',
    multiSelect: false,
    options: [
      { label: '內網 http (Recommended)', description: '流量留在 VPC 內' },
      { label: '公網 https，拿掉 IP 白名單', description: '對外暴露面變大' },
    ],
  },
]

test('daemon 的題目轉成畫面要用的形狀；壞的丟掉', () => {
  const got = toPendingQuestions(RAW)
  assert.equal(got.length, 1)
  assert.equal(got[0].header, '連線方式')
  assert.equal(got[0].options[1].label, '公網 https，拿掉 IP 白名單')
  assert.deepEqual(toPendingQuestions(null), [])
  assert.deepEqual(toPendingQuestions([{ header: 'x' }, 'bad', { question: '  ' }]), [], '沒有題目本文的不算')
})

test('畫面上已經有這題就不疊卡；只看到選項、沒有題目（pane 太矮）時要疊', () => {
  const pending = toPendingQuestions(RAW)
  assert.equal(questionVisibleOnScreen(pending, null), false, '畫面認不出問句')
  assert.equal(questionVisibleOnScreen(pending, '內網 http (Recommended)'), false, '只看到選項')
  // 畫面折行會在中間插空白。
  assert.equal(questionVisibleOnScreen(pending, 'prod console 連 prod RPA 要走\n哪條路？（現在失敗是因為…'), true)
})

const TWO = toPendingQuestions([
  {
    question: '#253 預覽模式的三項實機驗收都過了。要關票嗎？',
    header: '#253 關票',
    options: [{ label: '關票 (Recommended)' }, { label: '先不關' }],
  },
  {
    question: '預覽面板的兩個小取捨要不要改？',
    header: '預覽 UI',
    multiSelect: true,
    options: [{ label: '改啟動鍵文字' }, { label: '啟動預覽不收摺疊' }],
  },
])
const choices = (...t: string[]) => [...t, 'Type something.', 'Chat about this'].map((title) => ({ title }))

test('畫面上是第幾題：有問句比問句，沒問句（pane 太矮）比選項', () => {
  assert.equal(pendingIndexOnScreen(TWO, { question: '#253 預覽模式的三項實機\n驗收都過了。要關票嗎？', choices: [] }), 0)
  assert.equal(pendingIndexOnScreen(TWO, { question: null, choices: choices('改啟動鍵文字', '啟動預覽不收摺疊') }), 1)
  assert.equal(pendingIndexOnScreen(TWO, { question: null, choices: choices('關票 (Recommended)', '先不關') }), 0)
})

test('畫面上是第幾題：選項只對上一部分、或兩題都對得上時不猜', () => {
  assert.equal(pendingIndexOnScreen(TWO, { question: null, choices: choices('改啟動鍵文字') }), -1, '只對上一半')
  assert.equal(pendingIndexOnScreen(TWO, { question: 'Ready to submit your answers?', choices: choices('Submit answers', 'Cancel') }), -1)
  const same = toPendingQuestions([
    { question: 'A？', options: [{ label: '好' }, { label: '不要' }] },
    { question: 'B？', options: [{ label: '好' }, { label: '不要' }] },
  ])
  assert.equal(pendingIndexOnScreen(same, { question: null, choices: choices('好', '不要') }), -1, '兩題選項一樣就不猜')
  assert.equal(pendingIndexOnScreen(TWO, null), -1)
  assert.equal(pendingIndexOnScreen([], { question: null, choices: choices('先不關') }), -1)
})

// ── Jev 的選項建議（issue #1475）：只提示；位置照 daemon 的原 questions[]／options[]，不重算門檻 ──

const JEV_RAW = [
  { question: '連線方式？', multiSelect: false, options: [{ label: '內網' }, { label: '公網' }] },
  { question: '要哪些？', multiSelect: true, options: [{ label: 'A' }, { label: 'B' }] },
  { question: '部署到哪？', options: [{ label: 'staging' }, { label: 'prod' }, { label: 'canary' }] },
]
const jevOpts = (questions: unknown[]) => ({ ledger_id: 'L1', model: 'jev-1.13.0', threshold: 0.7, margin: 0.15, questions })

test('Jev 建議掛到對應那題的對應選項，分數原樣', () => {
  const got = toPendingQuestions(JEV_RAW, jevOpts([
    { index: 0, scores: [0.91, 0.08], suggested: 0 },
    { index: 2, scores: [0.1, 0.2, 0.85], suggested: 2 },
  ]))
  assert.deepEqual(got[0].jev, { option: 0, label: '內網', score: 0.91 })
  assert.equal(got[1].jev, null, '沒列的題不標')
  assert.deepEqual(got[2].jev, { option: 2, label: 'canary', score: 0.85 })
})

test('沒有建議（suggested 是 null）、沒有 jev_options、舊 daemon：什麼都不掛', () => {
  const none = toPendingQuestions(JEV_RAW, jevOpts([{ index: 0, scores: [0.85, 0.8], suggested: null }]))
  assert.deepEqual(none.map((q) => q.jev), [null, null, null])
  assert.deepEqual(toPendingQuestions(JEV_RAW).map((q) => q.jev), [null, null, null])
  assert.deepEqual(toPendingQuestions(JEV_RAW, null).map((q) => q.jev), [null, null, null])
  assert.deepEqual(toPendingQuestions(JEV_RAW, 'bad').map((q) => q.jev), [null, null, null])
})

test('多選題不標，就算 daemon 帶了；分數壞掉、選項數對不上、位置超出範圍也不標', () => {
  const got = toPendingQuestions(JEV_RAW, jevOpts([
    { index: 1, scores: [0.9, 0.1], suggested: 0 },
    { index: 0, scores: [0.9], suggested: 0 },
  ]))
  assert.equal(got[1].jev, null, '多選題')
  assert.equal(got[0].jev, null, '分數數量跟選項對不上')
  for (const bad of [
    { index: 0, scores: [1.4, 0.1], suggested: 0 },
    { index: 0, scores: [0.9, 'x'], suggested: 0 },
    { index: 0, scores: [0.9, 0.1], suggested: 5 },
    { index: 0, scores: [0.9, 0.1], suggested: -1 },
    { index: 0, scores: [0.9, 0.1], suggested: 0.5 },
  ]) {
    assert.equal(toPendingQuestions(JEV_RAW, jevOpts([bad]))[0].jev, null, JSON.stringify(bad))
  }
})

test('前面的題目被丟掉、選項被濾掉時，建議仍掛在原位置那一項', () => {
  const raw = [
    { question: '  ', options: [{ label: 'x' }, { label: 'y' }] },
    { question: '選哪個？', options: [{ label: '' }, { label: '甲' }, { label: '乙' }] },
  ]
  const got = toPendingQuestions(raw, jevOpts([{ index: 1, scores: [0.0, 0.1, 0.9], suggested: 2 }]))
  assert.equal(got.length, 1)
  assert.deepEqual(got[0].options.map((o) => o.label), ['甲', '乙'])
  assert.deepEqual(got[0].jev, { option: 1, label: '乙', score: 0.9 })
  // 建議的是一個沒有 label 的選項：不掛。
  assert.equal(toPendingQuestions(raw, jevOpts([{ index: 1, scores: [0.9, 0.0, 0.1], suggested: 0 }]))[0].jev, null)
})

test('整份回應 {questions, jev_options} 的解法；舊形狀（只有 questions）照舊', () => {
  assert.deepEqual(
    toPendingQuestionsFromResponse({ questions: JEV_RAW, jev_options: jevOpts([{ index: 0, scores: [0.2, 0.9], suggested: 1 }]) })[0].jev,
    { option: 1, label: '公網', score: 0.9 },
  )
  assert.equal(toPendingQuestionsFromResponse({ questions: JEV_RAW })[0].jev, null)
  assert.deepEqual(toPendingQuestionsFromResponse({ questions: null, jev_options: null }), [])
  assert.deepEqual(toPendingQuestionsFromResponse(null), [])
})

test('pendingAtOnScreen 把建議帶給畫面上那一題；標題比對去空白、認得被截的 …', () => {
  const pending = toPendingQuestions(JEV_RAW, jevOpts([{ index: 0, scores: [0.91, 0.08], suggested: 0 }]))
  const at = pendingAtOnScreen(pending, { question: '連線方式？', choices: [{ title: '內網' }, { title: '公網' }] })
  assert.deepEqual(at?.jev, { option: 0, label: '內網', score: 0.91 })
  assert.equal(jevMatchesTitle('內網 http (Recommended)', '內網 http (Recommended)'), true)
  assert.equal(jevMatchesTitle('內網 http (Recommended)', '內網  http\n(Recommended)'), true, '折行插的空白不算')
  assert.equal(jevMatchesTitle('公網 https，拿掉 IP 白名單', '公網 https，拿掉…'), true, '被截斷的標題比前綴')
  assert.equal(jevMatchesTitle('內網 http', '公網 https'), false)
  assert.equal(jevMatchesTitle('內網', '…'), false)
  assert.equal(jevMatchesTitle('', ''), false)
})
