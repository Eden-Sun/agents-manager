import test from 'node:test'
import assert from 'node:assert/strict'
import { dispatchFrameForTest, resetStoreForTest, useStore } from './store.ts'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'

const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 })

// 同一個行程裡前面的測試檔（例如 daemon_seq 5000 的 storeActions）會把已套用的 seq 留得很高；macOS 上
// 檔案順序跟 Linux 不同，下面那份 daemon_seq: 1 的快照就被當舊的丟掉、測試逾時（#773）。
// 這個測試先把 seq 推高，等於不管檔案順序都重現那個狀況。
test('前面的測試留下較高的 daemon_seq 時，下一個測試仍要自己歸零', async () => {
  reset()
  routeDaemon(() => json({ daemon_seq: 5000, hosts: [], projects: [], bots: [], runs: [], turns: [] }))
  await useStore.getState().refreshState()
})

test('舊版 host_changed 只有 host 時重讀 state，讓身份登入狀態收斂', async () => {
  resetStoreForTest()
  reset()
  useStore.setState({
    localIdentityStatus: {
      cc1: { name: 'cc1', kind: 'claude', logged_in: false, reason: null, account: null, plan: null, source: 'config', config_dir: null },
    },
  })
  routeDaemon((request) =>
    request.path.endsWith('/state')
      ? json({
          daemon_seq: 1,
          hosts: [{ name: 'local', connected: true, identities: { cc1: { name: 'cc1', kind: 'claude', logged_in: true } } }],
          projects: [],
          bots: [],
          runs: [],
          turns: [],
        })
      : json({}),
  )

  let unsubscribe = () => {}
  const updated = new Promise<void>((resolve) => {
    unsubscribe = useStore.subscribe((state) => {
      if (state.localIdentityStatus.cc1?.logged_in === true) resolve()
    })
  })
  try {
    dispatchFrameForTest({ type: 'host_changed', data: { host: 'local' } })
    assert.ok(requests.some((request) => request.method === 'GET' && request.path.endsWith('/state')))
    await updated
    assert.equal(useStore.getState().localIdentityStatus.cc1?.logged_in, true)
  } finally {
    unsubscribe()
  }
})
