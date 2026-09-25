import { test } from 'node:test'
import assert from 'node:assert/strict'
import { pendingIndexOnScreen, questionVisibleOnScreen, toPendingQuestions } from './pendingQuestion'

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
