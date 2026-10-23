import test from 'node:test'
import assert from 'node:assert/strict'
import type { IdentityStatusMap, ToolMap, ToolStatus } from '../api/types'
import { onboardingModel } from './onboarding'

const missing: ToolStatus = { installed: false, path: null, version: null, logged_in: null }
const tool = (logged_in: boolean | null): ToolStatus => ({ installed: true, path: '/bin/x', version: '1.0.0', logged_in })
const tools = (o: Partial<ToolMap> = {}): ToolMap => ({ claude: missing, codex: missing, grok: missing, agy: missing, ...o })
const base = { identities: {} as IdentityStatusMap, herdrConnected: true, projectCount: 1 }

test('一個 CLI 都沒裝：要顯示，下一步是裝 CLI，沒有任何一種就緒', () => {
  const m = onboardingModel({ ...base, tools: tools(), projectCount: 0 })
  assert.equal(m.needed, true)
  assert.equal(m.anyReady, false)
  assert.equal(m.next, 'cli')
  assert.deepEqual(m.rows.map((r) => r.state), ['missing', 'missing', 'missing', 'missing'])
})

test('已安裝但未登入不算就緒；登入後才進到「新增 Project」', () => {
  assert.equal(onboardingModel({ ...base, tools: tools({ claude: tool(false) }), projectCount: 0 }).next, 'cli')
  const m = onboardingModel({ ...base, tools: tools({ claude: tool(true) }), projectCount: 0 })
  assert.equal(m.next, 'project')
  assert.equal(m.needed, true)
})

test('logged_in === null 是問不到，不是未登入：已安裝就算可用，狀態另列 unknown', () => {
  const m = onboardingModel({ ...base, tools: tools({ grok: tool(null) }) })
  assert.equal(m.rows[2].state, 'unknown')
  assert.equal(m.anyReady, true)
  assert.equal(m.needed, false)
})

test('預設帳號未登入但有已登入的命名身分：這個 kind 算已登入', () => {
  const identities = { cc1: { name: 'cc1', kind: 'claude', logged_in: true, reason: null, account: 'me@x.com' } } as unknown as IdentityStatusMap
  const m = onboardingModel({ ...base, tools: tools({ claude: tool(false) }), identities })
  assert.equal(m.rows[0].state, 'ok')
  assert.equal(m.rows[0].account, 'me@x.com')
})

test('有 Project、有就緒的 CLI、herdr 連著：自動收起；herdr 斷線或沒有 Project 又出現', () => {
  const ok = { ...base, tools: tools({ codex: tool(true) }) }
  assert.equal(onboardingModel(ok).needed, false)
  assert.equal(onboardingModel(ok).next, 'done')
  assert.equal(onboardingModel({ ...ok, herdrConnected: false }).next, 'herdr')
  assert.equal(onboardingModel({ ...ok, projectCount: 0 }).next, 'project')
})

test('pending 數：缺的／未登入的 CLI 與斷線的 herdr', () => {
  assert.equal(onboardingModel({ ...base, tools: tools({ claude: tool(true), codex: tool(false) }) }).pending, 3)
  assert.equal(onboardingModel({ ...base, tools: tools({ claude: tool(true), codex: tool(true), grok: tool(true), agy: tool(true) }), herdrConnected: false }).pending, 1)
})
