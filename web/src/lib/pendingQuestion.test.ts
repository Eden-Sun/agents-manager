import { test } from 'node:test'
import assert from 'node:assert/strict'
import { questionVisibleOnScreen, toPendingQuestions } from './pendingQuestion'

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
