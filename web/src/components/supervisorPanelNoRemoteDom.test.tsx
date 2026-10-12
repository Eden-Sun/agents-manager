/**
 * #1477：手機 Remote Control 已廢。總管面板不再畫「遠端入口」那一塊——就算舊 daemon 的 payload 還帶著
 * `remote: {status:'verified', url}`，畫面上也不能出現狀態燈、連線連結或 Remote Control 的說明。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import { SupervisorPanel } from './SupervisorPanel'

before(setupDom)
afterEach(unmountAll)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const payload = {
  configured: true,
  bot_id: 'bot-agm',
  project_id: 'proj-agm',
  model: 'fable',
  identity: 'cc0',
  effort: 'low',
  status: 'running',
  // 舊 daemon 還會回的欄位：新前端要忽略它。
  remote: { status: 'verified', url: 'https://claude.ai/code/session_1', observed_by: 'user', capability: { status: 'supported' } },
  pending_count: 0,
  assignments: [],
}

test('總管面板不畫遠端入口：狀態、連結、說明都沒有', async () => {
  mockApi({
    async request(_method, path) {
      if (path === '/supervisor') return payload
      if (path === '/supervisor/incidents') return { incidents: [] }
      return {}
    },
    async upload() {
      return {}
    },
  })
  await mount(<SupervisorPanel />)
  await until(() => document.querySelector('.agm-panel .agm-name') !== null, '面板讀完狀態')

  const text = document.querySelector('.agm-panel')!.textContent ?? ''
  assert.doesNotMatch(text, /遠端入口|Remote Control|已確認可用|開啟連線/, `不該出現遠端入口：${text}`)
  assert.equal(document.querySelector('a.agm-link'), null, '沒有任何連線連結')
  assert.match(text, /AGM/, '面板本身照常畫')
})
