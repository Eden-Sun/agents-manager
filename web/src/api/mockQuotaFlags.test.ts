/** mock 的額度讀數要帶 daemon 算好的 `low`／`critical`（API.md §12.4：剩餘 <30% 是 low、<5% 是 critical），前端只讀旗標、不自己算。 */
import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { useManualMockClock } from './mockClock.ts'

interface Win { used_pct: number; low?: boolean; critical?: boolean }

before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
const mock = new MockTransport()

test('GET /api/quota：剩餘不到 30% 的窗標 low（seed 的 claude:cc1 5h 已用 85%）', async () => {
  const out = (await mock.request('GET', '/quota')) as { kinds: Record<string, { five_hour: Win | null }> }
  const w = out.kinds['claude:cc1'].five_hour
  assert.ok(w, 'seed 要有 claude:cc1 的 5h 窗')
  assert.equal(w.low, true)
  assert.equal(w.critical, false)
})

test('GET /api/quota：剩餘還高的窗 low／critical 都是 false（seed 的 claude 5h 已用 18%）', async () => {
  const out = (await mock.request('GET', '/quota')) as { kinds: Record<string, { five_hour: Win | null }> }
  const w = out.kinds.claude.five_hour
  assert.ok(w)
  assert.equal(w.low, false)
  assert.equal(w.critical, false)
})
