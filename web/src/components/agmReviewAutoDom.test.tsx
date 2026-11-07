import test, { before, after, afterEach } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { AgmReviewBox } from './AgmReviewBox'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

test('開框自動分析後直接顯示結論，POST 帶 auto 與完整版本範圍', { timeout: 30_000 }, async () => {
  let completed = false
  const requests = fakeApi((req) => {
    if (req.path.includes('/release-triage')) return { rows: [] }
    if (!req.path.includes('/claude-update/review')) return undefined
    if (req.method === 'POST') completed = true
    return { version: '2.0', auto: true, review: completed ? { state: 'done', source: 'api', result: '自動分析完成（未實測）' } : { state: 'none' } }
  })
  await mount(<AgmReviewBox kind="codex" host="local" from="1.0" to="2.0" refreshKey={0} />)
  await until(() => document.body.textContent?.includes('自動分析完成') === true, '自動結論顯示')
  const posts = requests.filter((r) => r.method === 'POST')
  assert.equal(posts.length, 1)
  assert.deepEqual(posts[0].body, { kind: 'codex', host: 'local', from: '1.0', to: '2.0', auto: true })
})
