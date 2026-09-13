import test from 'node:test'
import assert from 'node:assert/strict'
import { chipTracked } from './supervisorProject.ts'

/** 總管專案的 id（實機 `GET /api/supervisor` 回的那個）。 */
const SUP = '01M23DD13WT45KX1JKKTEMMBB6'

/** 2026-09-13 實機那顆：AGM 開出去的瀏覽器殭屍清理工，跑完卻進了「剛跑完」。 */
const browserGc = { pending: null, parent_bot_id: null, team: null, project_id: SUP }
const userBot = { pending: null, parent_bot_id: null, team: null, project_id: '01M1PROJECT' }

test('總管專案底下的 bot 不進晶片列（AGM 本人與它開出去的工人都在這個專案）', () => {
  assert.equal(chipTracked(browserGc, SUP), false)
  assert.equal(chipTracked(userBot, SUP), true)
})

test('專案改名之後仍然排除——認的是 id 不是名字', () => {
  // 使用者把總管專案從 `AGM` 改名成 `AGM-DM-GRUP`（再改成別的也一樣）：id 不會變，
  // 所以這顆照樣不進晶片列。名字比對在這種情況下就會破。
  assert.equal(chipTracked({ ...browserGc, project_id: SUP }, SUP), false)
  // 名字很像總管、但不是那個專案的，照樣要顯示
  assert.equal(chipTracked({ ...userBot, project_id: 'AGM-looking-but-not-it' }, SUP), true)
})

test('讀不到總管的 project_id 時退回「不排除」，不會把使用者的 bot 弄不見', () => {
  assert.equal(chipTracked(userBot, null), true)
  assert.equal(chipTracked(browserGc, null), true)
})

test('子 agent、team 成員、還沒建好的一樣不進晶片列（原本的規則沒有動）', () => {
  assert.equal(chipTracked({ ...userBot, parent_bot_id: 'mother' }, SUP), false)
  assert.equal(chipTracked({ ...userBot, team: { id: 't1' } }, SUP), false)
  assert.equal(chipTracked({ ...userBot, pending: true }, SUP), false)
})
