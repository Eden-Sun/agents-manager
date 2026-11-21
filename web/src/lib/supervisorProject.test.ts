import test from 'node:test'
import assert from 'node:assert/strict'
import { chipTracked } from './supervisorProject.ts'

const SUP = '01M23DD13WT45KX1JKKTEMMBB6'

const browserGc = { pending: null, parent_bot_id: null, project_id: SUP }
const userBot = { pending: null, parent_bot_id: null, project_id: '01M1PROJECT' }

test('總管專案底下的 bot 不進晶片列（AGM 本人與它開出去的工人都在這個專案）', () => {
  assert.equal(chipTracked(browserGc, SUP), false)
  assert.equal(chipTracked(userBot, SUP), true)
})

test('專案改名之後仍然排除——認的是 id 不是名字', () => {
  assert.equal(chipTracked({ ...browserGc, project_id: SUP }, SUP), false)
  assert.equal(chipTracked({ ...userBot, project_id: 'AGM-looking-but-not-it' }, SUP), true)
})

test('讀不到總管的 project_id 時退回「不排除」，不會把使用者的 bot 弄不見', () => {
  assert.equal(chipTracked(userBot, null), true)
  assert.equal(chipTracked(browserGc, null), true)
})

test('子 agent、還沒建好的一樣不進晶片列（原本的規則沒有動）', () => {
  assert.equal(chipTracked({ ...userBot, parent_bot_id: 'mother' }, SUP), false)
  assert.equal(chipTracked({ ...userBot, pending: true }, SUP), false)
})

test('專家模式的執行者／顧問不進晶片列，管理者照算（成員取代 child，沒有 parent_bot_id）', () => {
  assert.equal(chipTracked({ ...userBot, role: 'executor' }, SUP), false)
  assert.equal(chipTracked({ ...userBot, role: 'advisor' }, SUP), false)
  assert.equal(chipTracked({ ...userBot, role: 'manager' }, SUP), true)
  assert.equal(chipTracked({ ...userBot, role: null }, SUP), true)
})

test('chipTracked: specialist 與 executor／advisor 一樣不進晶片列', () => {
  const spec = { pending: false, parent_bot_id: null, project_id: 'p1', role: 'specialist' as const }
  assert.equal(chipTracked(spec, null), false)
})
