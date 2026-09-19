import test from 'node:test'
import assert from 'node:assert/strict'
import { composerPlaceholder, sendButtonLabel, sendButtonTitle } from './composerLabels.ts'

const idle = { disabled: false, reason: '', queued: false }
const running = { disabled: false, reason: '這回合還在跑，送出會排到結束後', queued: true }
const stopped = { disabled: false, reason: 'Bot 沒在跑：送出會先啟動它，起來後自動送出', queued: true, autoStart: true }
const starting = { disabled: false, reason: '啟動中，起來後自動送出', queued: true }

/**
 * #122 之後沒在跑的 bot 是 `queued:true＋autoStart:true`，輸入框與送出鍵卻沿用「回合還在跑、送出會排隊」那套字——
 * 對一顆停著的 bot 說「這回合還在跑」、按鈕寫「排隊送出」（真畫面：AM-1-L 離線，狀態列說「Bot 沒在跑」，輸入框說「這回合還在跑」）。
 */
test('沒在跑的 bot（autoStart）：不說「這回合還在跑」、送出鍵說會先啟動', () => {
  assert.doesNotMatch(composerPlaceholder(stopped, { phone: false, starting: false }), /還在跑/)
  assert.match(composerPlaceholder(stopped, { phone: false, starting: false }), /啟動/)
  assert.equal(sendButtonLabel(stopped, { phone: false, sending: false, uploading: false }), '啟動並送出')
  assert.equal(sendButtonLabel(stopped, { phone: true, sending: false, uploading: false }), '啟動送出')
  assert.doesNotMatch(sendButtonTitle(stopped, false) ?? '', /這回合結束/)
  assert.match(sendButtonTitle(stopped, false) ?? '', /啟動/)
})

test('回合中排隊、在等 bot 起來的排隊、閒著：字跟原本一樣', () => {
  assert.match(composerPlaceholder(running, { phone: false, starting: false }), /這回合還在跑/)
  assert.match(composerPlaceholder(starting, { phone: false, starting: true }), /bot 起來/)
  assert.equal(composerPlaceholder(running, { phone: true, starting: false }), '下一則訊息…')
  assert.equal(sendButtonLabel(running, { phone: false, sending: false, uploading: false }), '排隊送出')
  assert.equal(sendButtonLabel(running, { phone: true, sending: false, uploading: false }), '排隊')
  assert.equal(sendButtonLabel(idle, { phone: false, sending: false, uploading: false }), '送出')
  assert.equal(sendButtonTitle(running, false), '這回合結束後自動送出')
  assert.equal(sendButtonTitle(idle, false), undefined)
})

test('送出中／上傳中優先於其他字；鎖住時輸入框講原因', () => {
  assert.equal(sendButtonLabel(stopped, { phone: false, sending: true, uploading: false }), '送出中…')
  assert.equal(sendButtonLabel(running, { phone: false, sending: false, uploading: true }), '上傳中…')
  assert.equal(sendButtonTitle(stopped, true), '附件上傳中…')
  const locked = { disabled: true, reason: 'Run 狀態為 starting，尚無法送出訊息', queued: false }
  assert.match(composerPlaceholder(locked, { phone: false, starting: false }), /Run 狀態為 starting.*可以先打/)
})
