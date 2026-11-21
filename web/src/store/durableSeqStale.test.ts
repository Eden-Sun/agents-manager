import test from 'node:test'
import assert from 'node:assert/strict'
import { useStore } from './store.ts'
import { reset, routeDaemon } from './storeEnv.harness.ts'

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
const snapshot = (daemon_seq: number) => ({
  daemon_seq,
  hosts: [],
  projects: [],
  bots: [],
  runs: [],
  turns: [],
})

test('快照比在飛期間已套用的耐久幀舊：耐久 seq 不往回掉（重連的 since 不重送已套用的幀）', async () => {
  reset()
  useStore.setState({ lastSeq: 1000, lastDurableSeq: 1000 })
  let fetches = 0
  routeDaemon((req) => {
    if (!req.path.endsWith('/state')) return json({ messages: [], turns: [], has_more: false })
    fetches += 1
    if (fetches === 1) {
      useStore.setState({ lastSeq: 1001, lastDurableSeq: 1001 })
      return json(snapshot(1000))
    }
    return json({ error: 'internal' }, 500)
  })
  await useStore.getState().refreshState()
  assert.equal(fetches, 2, '比快照新的耐久幀已經到了，要補抓一次')
  assert.equal(useStore.getState().lastDurableSeq, 1001)
})

test('daemon 重啟（快照 seq 比請求送出時小）：耐久 seq 跟著降到快照值', async () => {
  reset()
  useStore.setState({ lastSeq: 5000, lastDurableSeq: 5000 })
  routeDaemon((req) => {
    if (!req.path.endsWith('/state')) return json({ messages: [], turns: [], has_more: false })
    return json(snapshot(20))
  })
  await useStore.getState().refreshState()
  assert.equal(useStore.getState().lastDurableSeq, 20)
})
