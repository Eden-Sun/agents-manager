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

test('host_changed 帶 baseline：本機與遠端都更新；沒帶＝沒變；issues: null 是未知不是全缺', () => {
  reset()
  useStore.setState({
    hosts: [
      {
        name: 'm4p', ssh: 'm4p', ssh_port: 22, herdr_session: 's', remote_path: '', connected: true, error: null,
        disconnected_since: null, attach_command: '', herdr: useStore.getState().localHerdr, tools: useStore.getState().localTools,
        baseline: null, identity_status: {},
      },
    ],
  })
  const base = { checked_at: '2026-10-02T00:00:00Z', os: 'Linux' }

  dispatchFrameForTest({ type: 'host_changed', data: { name: 'local', connected: true, baseline: { ...base, issues: [{ id: 'tool.rtk', severity: 'critical', message: '缺工具 rtk' }] } } })
  assert.equal(useStore.getState().localBaseline?.issues?.[0]?.id, 'tool.rtk')

  dispatchFrameForTest({ type: 'host_changed', data: { name: 'local', connected: true } })
  assert.equal(useStore.getState().localBaseline?.issues?.length, 1, '沒帶 baseline 不能把已知的差異洗掉')

  dispatchFrameForTest({ type: 'host_changed', data: { name: 'm4p', connected: true, baseline: { ...base, issues: null } } })
  assert.equal(useStore.getState().hosts[0].baseline?.issues, null, '探測沒跑完＝未知')

  dispatchFrameForTest({ type: 'host_changed', data: { name: 'm4p', connected: true, baseline: { ...base, issues: [] } } })
  assert.deepEqual(useStore.getState().hosts[0].baseline?.issues, [])
})
