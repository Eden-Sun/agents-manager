import test, { before, after, afterEach } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, setupDom, teardownDom, unmountAll, until, click } from '../testing/domHarness'
import { AgmReviewBox } from './AgmReviewBox'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

test('四種 CLI 投影分診與 publish 重試，不讀 changelog-review', { timeout: 30000 }, async () => {
  for (const kind of ['claude', 'codex', 'agy', 'grok']) {
    let published = false
    const requests = fakeApi((req) => {
      assert.ok(!req.path.includes('/claude-update/review'))
      if (req.method === 'POST') {
        assert.ok(req.path.endsWith('/release-triage/publish'))
        assert.deepEqual(req.body, { kind, version: '2.0' })
        published = true
        return {}
      }
      return { repo: 'org/repo', rows: [{
        version: '2.0', status: published ? 'published' : 'judged',
        publish_error: published ? null : 'GitHub unavailable', entries: [],
        verdicts: { issues: [
          { entry_ids: ['a'], title: '更新整合', triage: 'adopt' },
          { entry_ids: ['b'], title: '既有修正', triage: 'guard', duplicate_of: 42 },
        ] },
        issues: published ? [
          { entry_ids: ['a'], number: 41, url: 'https://github.com/org/repo/issues/41', comment: false },
          { entry_ids: ['b'], number: 42, url: 'https://github.com/org/repo/issues/42', comment: true },
        ] : [],
      }] }
    })
    const root = await mount(<AgmReviewBox kind={kind} host="local" from="1.0" to="2.0" refreshKey={0} />)
    await until(() => root.textContent?.includes('GitHub unavailable') === true, '失敗原因')
    assert.equal(requests.filter((r) => r.method === 'POST').length, 0)
    await click(root.querySelector('button')!)
    await until(() => root.querySelector('a')?.textContent?.includes('#41') === true, '發布結果')
    assert.ok(root.textContent?.includes('留言於 #42'))
    assert.equal(root.querySelector('a')?.getAttribute('href'), 'https://github.com/org/repo/issues/41')
    assert.ok(!root.textContent?.includes('GitHub unavailable'))
    await unmountAll()
  }
})

test('輪詢刷新分診結果，卸載停止輪詢', { timeout: 30000 }, async () => {
  let complete = false
  let reads = 0
  fakeApi(() => {
    reads++
    return { rows: [{ version: '2.0', status: complete ? 'empty' : 'dispatched', entries: [] }] }
  })
  const root = await mount(<AgmReviewBox kind="codex" host="local" from="1.0" to="2.0" refreshKey={0} />)
  await until(() => root.textContent?.includes('分診中') === true, '分診中')
  complete = true
  await until(() => root.textContent?.includes('看過了，沒有要處理的') === true, '輪詢刷新', 10000)
  await unmountAll()
  const count = reads
  await new Promise((resolve) => setTimeout(resolve, 5100))
  assert.equal(reads, count)
})
