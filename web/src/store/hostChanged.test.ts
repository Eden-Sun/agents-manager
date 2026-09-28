import test from 'node:test'
import assert from 'node:assert/strict'
import { dispatchFrameForTest, useStore } from './store.ts'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'

const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 })

test('舊版 host_changed 只有 host 時重讀 state，讓身份登入狀態收斂', async () => {
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
