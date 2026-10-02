import test from 'node:test'
import assert from 'node:assert/strict'
import { useStore } from './store.ts'
import { reset, routeDaemon } from './storeEnv.harness.ts'

const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 })
const snapshot = (daemon_seq: number, projectId: string) => ({
  daemon_seq,
  hosts: [],
  projects: [{ id: projectId, label: projectId, path: `/${projectId}`, host: 'local' }],
  bots: [],
  runs: [],
  turns: [],
})

// #773：store 是模組單例，bun 整樹同一個行程。前面的測試檔把 daemon_seq 推高後，後面用 daemon_seq: 1 的快照
// 會被當舊的丟掉、測試逾時——只在檔案順序不同的平台（macOS）現形。`reset()` 要負責把 seq 歸零，
// 這條測試先把 seq 推高，不管檔案順序都守住這件事。
test('reset() 把已套用的 daemon_seq 歸零：前面的測試推高過也照樣收下 daemon_seq: 1', async () => {
  reset()
  routeDaemon(() => json(snapshot(5000, 'high')))
  await useStore.getState().refreshState()
  assert.equal(useStore.getState().projects[0]?.id, 'high')

  reset()
  routeDaemon(() => json(snapshot(1, 'low')))
  await useStore.getState().refreshState()
  assert.equal(useStore.getState().projects[0]?.id, 'low')
})
