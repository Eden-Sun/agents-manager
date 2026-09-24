/**
 * issue #531：連不上 daemon 時 `fetchRebuildRequests` 會把 `TypeError: Failed to fetch` 往外拋，
 * `RebuildBadge` 的輪詢沒接 → 每 30 秒一則 unhandled rejection，chip 還停在斷線前的數字。
 * 這裡釘住三種結果分得開：有名單／daemon 說沒有／問不到。
 *
 * 自己架假 daemon，不借 `store/storeEnv.harness.ts`：跨目錄 import 它會多出第二份模組實例，
 * 裝上去的 `fetch` 與別的測試檔呼叫的 `routeDaemon` 對不到同一份 route，整批 store 測試會收到預設 200。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { fetchRebuildRequests } from './rebuildRequests.ts'

const g = globalThis as unknown as Record<string, unknown>
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

const approval = (id: string) => ({
  id,
  purpose: 'rebuild',
  requester: 'bot-1',
  target_commit: 'abc1234',
  scope: '修 X',
  status: 'pending',
  created_at: new Date().toISOString(),
})

/** 這一段用這個假 daemon；跑完一定還原，別把全域留給下一個測試檔。 */
async function withDaemon<T>(handler: (path: string) => Response, body: () => Promise<T>): Promise<T> {
  const saved = { fetch: g.fetch, location: g.location, localStorage: g.localStorage }
  g.location = { protocol: 'http:', host: '127.0.0.1:7788' }
  g.localStorage = { getItem: () => null, setItem: () => {}, removeItem: () => {} }
  g.fetch = async (input: string) => handler(String(input).split('?')[0])
  try {
    return await body()
  } finally {
    g.fetch = saved.fetch
    g.location = saved.location
    g.localStorage = saved.localStorage
  }
}

const session = (path: string) => (path === '/api/session' ? json({ token: 't' }) : null)

test('daemon 有回：名單照回，不算斷線', async () => {
  const snap = await withDaemon(
    (path) => session(path) ?? (path.endsWith('/supervisor/approvals') ? json({ approvals: [approval('a1')] }) : json({})),
    fetchRebuildRequests,
  )
  assert.equal(snap.offline, false)
  assert.deepEqual(snap.rows?.map((r) => r.id), ['a1'])
})

test('daemon 回錯（舊 daemon 沒這支、500…）：收掉 chip，但不是斷線', async () => {
  for (const status of [404, 405, 500, 403]) {
    const snap = await withDaemon(
      (path) => session(path) ?? (path.endsWith('/supervisor/approvals') ? json({ error: 'nope' }, status) : json({})),
      fetchRebuildRequests,
    )
    assert.deepEqual(snap, { rows: null, offline: false }, `status=${status}`)
  }
})

test('連不上 daemon：不往外拋，標成 offline，讓 badge 自己決定留不留舊數字', async () => {
  const snap = await withDaemon(
    () => {
      throw new TypeError('Failed to fetch')
    },
    fetchRebuildRequests,
  )
  assert.deepEqual(snap, { rows: null, offline: true })
})

test('請求被中止也算問不到，不是 daemon 說沒有', async () => {
  const snap = await withDaemon(
    () => {
      const e = new Error('The operation was aborted')
      e.name = 'AbortError'
      throw e
    },
    fetchRebuildRequests,
  )
  assert.deepEqual(snap, { rows: null, offline: true })
})
