/**
 * 群組任務清單第一次載入失敗（daemon 換版的 502、逾時）之後，正在看的專案要能再載回來：
 * 以前 `refreshLoadedMissions` 與 `mission_updated` 只重載「清單載過」的專案，任務列就永遠不出現。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon } from './storeEnv.harness.ts'

const { useStore, dispatchFrameForTest } = await import('./store.ts')

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
const settle = () => new Promise((r) => setTimeout(r, 40))

test('清單第一次載入失敗：正在看的專案在 resync 後重載', async () => {
  reset()
  let fail = true
  routeDaemon((r) => {
    if (r.path.includes('/projects/p1/missions')) return fail ? json({ error: 'boom' }, 500) : json({ missions: [] })
    // 專案要在 state 裡，不然 refreshState 會把 selectedProjectId 清掉。
    if (r.path.endsWith('/state')) return json({ daemon_seq: 1, projects: [{ id: 'p1', label: 'p', path: '/p', host: 'local', bots: [] }], bots: [], runs: [], turns: [] })
    return json({ disabled: [], messages: [], turns: [], has_more: false })
  })
  await useStore.getState().loadMissions('p1')
  assert.equal(useStore.getState().missions.p1, undefined, '第一次載入失敗：清單是空的')
  fail = false
  useStore.setState({ selectedProjectId: 'p1' })
  dispatchFrameForTest({ type: 'resync', seq: 5 })
  await settle()
  assert.ok(Array.isArray(useStore.getState().missions.p1), 'resync 之後正在看的專案要重載成功')
})

test('mission_updated 來了：正在看的專案就算清單沒載成功也重載', async () => {
  reset()
  let fail = true
  const hits: string[] = []
  routeDaemon((r) => {
    if (r.path.includes('/projects/p1/missions')) {
      hits.push(r.path)
      return fail ? json({ error: 'boom' }, 500) : json({ missions: [] })
    }
    return json({})
  })
  await useStore.getState().loadMissions('p1')
  fail = false
  hits.length = 0
  useStore.setState({ selectedProjectId: 'p1' })
  dispatchFrameForTest({ type: 'mission_updated', seq: 6, data: { mission_id: 'm1', project_id: 'p1' } })
  await settle()
  assert.ok(hits.length > 0, '正在看的專案收到 mission_updated 要重載清單')
})

test('沒在看的專案不主動載', async () => {
  reset()
  let fail = true
  const hits: string[] = []
  routeDaemon((r) => {
    if (r.path.includes('/projects/p1/missions')) {
      hits.push(r.path)
      return fail ? json({ error: 'boom' }, 500) : json({ missions: [] })
    }
    return json({})
  })
  await useStore.getState().loadMissions('p1')
  fail = false
  hits.length = 0
  useStore.setState({ selectedProjectId: null })
  dispatchFrameForTest({ type: 'mission_updated', seq: 6, data: { mission_id: 'm1', project_id: 'p1' } })
  await settle()
  assert.equal(hits.length, 0, '沒在看、清單也沒載過的專案不主動去載')
})
