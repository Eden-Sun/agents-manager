import test from 'node:test'
import assert from 'node:assert/strict'
import { chipTracked, isSupervisorProject } from './supervisorProject.ts'

test('daemon 的總管專案與它開出去的雜務專案都算總管環境', () => {
  assert.equal(isSupervisorProject('AGM'), true)
  // 2026-09-13 實機：agm-pxf2pv-browser-gc 掛在這個專案底下，跑完卻出現在「剛跑完」
  assert.equal(isSupervisorProject('AGM-DM-GRUP'), true)
  assert.equal(isSupervisorProject('AGM-'), true)
})

test('使用者自己的專案不會被吃掉：只認前綴，不是「名字裡有 AGM」', () => {
  assert.equal(isSupervisorProject('my-AGM-tools'), false)
  assert.equal(isSupervisorProject('AGMtools'), false)
  assert.equal(isSupervisorProject('agm'), false)
  assert.equal(isSupervisorProject('agents-manager'), false)
  assert.equal(isSupervisorProject(''), false)
})

/** 2026-09-13 實機那顆：AGM 開出去的瀏覽器殭屍清理工，跑完卻進了「剛跑完」。 */
const browserGc = { pending: null, parent_bot_id: null, team: null, project_id: 'p-agm-dm-grup' }
const userBot = { pending: null, parent_bot_id: null, team: null, project_id: 'p-user' }

test('總管環境底下的 bot 不進晶片列——這就是 browser-gc 跑完會冒出來的那個洞', () => {
  const labels = [
    { id: 'p-agm', label: 'AGM' },
    { id: 'p-agm-dm-grup', label: 'AGM-DM-GRUP' },
    { id: 'p-user', label: 'agents-manager' },
  ]
  const ids = labels.filter((p) => isSupervisorProject(p.label)).map((p) => p.id)
  assert.equal(chipTracked(browserGc, ids), false)
  // 使用者自己的 bot 照樣要進來，不能順手把整列關掉
  assert.equal(chipTracked(userBot, ids), true)
  // 只比對 `=== 'AGM'`（改之前）時那顆會被當成一般 bot——這就是回報的現象
  const before = labels.filter((p) => p.label === 'AGM').map((p) => p.id)
  assert.equal(chipTracked(browserGc, before), true)
})

test('子 agent、team 成員、還沒建好的一樣不進晶片列（原本的規則沒有動）', () => {
  assert.equal(chipTracked({ ...userBot, parent_bot_id: 'mother' }, []), false)
  assert.equal(chipTracked({ ...userBot, team: { id: 't1' } }, []), false)
  assert.equal(chipTracked({ ...userBot, pending: true }, []), false)
})
