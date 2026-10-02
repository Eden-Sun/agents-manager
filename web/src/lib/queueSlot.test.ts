import test from 'node:test'
import assert from 'node:assert/strict'
import { queueSlotNotice, slotHolderFrom } from './queueSlot'

test('daemon 的 holder 欄位：認得的種類照收，不認得的當 unknown，沒有（舊 daemon）＝null', () => {
  assert.deepEqual(slotHolderFrom({ holder: { kind: 'agm', bot_name: 'AGM' } }), { kind: 'agm', botName: 'AGM' })
  assert.deepEqual(slotHolderFrom({ holder: { kind: 'user' } }), { kind: 'user', botName: null })
  assert.deepEqual(slotHolderFrom({ holder: { kind: 'martian' } }), { kind: 'unknown', botName: null })
  assert.equal(slotHolderFrom({}), null)
  assert.equal(slotHolderFrom({ holder: 'agm' }), null)
})

test('每種佔著的人都有清楚的中文說明，而且都說輸入框的字還在', () => {
  const agm = queueSlotNotice({ kind: 'agm', botName: 'AGM' }, 0)
  assert.ok(agm.includes('AGM') && agm.includes('交辦') && agm.includes('輸入框'), agm)
  const bot = queueSlotNotice({ kind: 'bot', botName: 'build' }, 0)
  assert.ok(bot.includes('build') && bot.includes('輸入框'), bot)
  assert.ok(queueSlotNotice({ kind: 'start', botName: null }, 0).includes('啟動'))
  assert.ok(queueSlotNotice({ kind: 'daemon', botName: null }, 0).includes('daemon'))
  // 舊文案不變（別的測試與使用者習慣）：自己的另一則在排隊，附件要重新加。
  assert.equal(queueSlotNotice({ kind: 'user', botName: null }, 2), '已有一則訊息排隊中，這一則已退回輸入框；2 個附件要重新加')
  assert.equal(queueSlotNotice(null, 0), '已有一則訊息排隊中，這一則已退回輸入框')
  assert.ok(queueSlotNotice({ kind: 'agm', botName: 'AGM' }, 1).includes('1 個附件'))
})
