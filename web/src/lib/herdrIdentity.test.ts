import test from 'node:test'
import assert from 'node:assert/strict'
import { herdrIdentity, splitAgentTail } from './herdrIdentity.ts'

test('有 active run：pane 與 herdr 名都給，tooltip 兩個都寫', () => {
  assert.deepEqual(herdrIdentity({ pane_id: 'w16T:p3' }, 'pt-hub-n4xznj'), {
    pane: 'w16T:p3',
    agent: 'pt-hub-n4xznj',
    text: 'w16T:p3 · pt-hub-n4xznj',
    title: 'herdr agent pt-hub-n4xznj・pane w16T:p3',
  })
})

test('沒有 run 就只剩暱稱：不給 herdr 名（bot.agent_name 此時是算出來的預設名，不是真的 pane）', () => {
  assert.equal(herdrIdentity(null, 'pt-hub-n4xznj'), null)
  assert.equal(herdrIdentity(undefined, 'pt-hub-n4xznj'), null)
})

test('run 還沒拿到 pane 或 agent 名時只給有的那個；都沒有就不給', () => {
  assert.equal(herdrIdentity({ pane_id: null }, 'pt-hub-n4xznj')?.text, 'pt-hub-n4xznj')
  assert.equal(herdrIdentity({ pane_id: null }, 'pt-hub-n4xznj')?.title, 'herdr agent pt-hub-n4xznj')
  assert.equal(herdrIdentity({ pane_id: 'w1:p2' }, '')?.text, 'w1:p2')
  assert.equal(herdrIdentity({ pane_id: 'w1:p2' }, '')?.title, 'pane w1:p2')
  assert.equal(herdrIdentity({ pane_id: ' ' }, null), null)
})

test('標題列截斷保住尾巴 6 碼：id 尾碼、子 agent 字尾', () => {
  assert.deepEqual(splitAgentTail('pt-hub-n4xznj'), ['pt-hub-', 'n4xznj'])
  assert.deepEqual(splitAgentTail('agents-manager-kd61te-i572'), ['agents-manager-kd61t', 'e-i572'])
  assert.deepEqual(splitAgentTail('b-x1'), ['', 'b-x1'])
})
