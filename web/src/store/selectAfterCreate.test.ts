/**
 * 新增／分身／fork／升級 bot 之後，主畫面要切到新 bot（#1339）：以前四條路只 `set({ selectedBotId })`，
 * 所以群組時間軸、整頁 shell、設定卡、終端分頁會留著舊的狀態，新 bot 看不到。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Project } from '../api/types.ts'

const { useStore } = await import('./store.ts')

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
/**
 * 等條件成立：每一輪讓微任務與 setImmediate 跑完，最多 `rounds` 輪；不靠時間（#1428）。
 * 等不到就帶上 `what` 失敗，不會像固定 40ms 那樣在負載高時假紅。
 */
async function settleUntil(cond: () => boolean, what: string, rounds = 200) {
  for (let i = 0; i < rounds; i++) {
    if (cond()) return
    await new Promise<void>((resolve) => setImmediate(resolve))
  }
  assert.fail(`等不到：${what}`)
}
const messagesRequested = (botId: string) => () => requests.some((r) => r.path.includes(`/bots/${botId}/messages`))
const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project
const bot = (id: string) => ({ id, name: id, project_id: 'p1', kind: 'claude', identity: null }) as Bot
const shell = { host: 'local', paneId: 'w1:p1', cwd: '' }

/** `/state` 回含新 bot 的快照；`/messages` 回空頁；建立、分身、fork、升級各回它們的 `bot_id`。 */
function route(newId: string) {
  reset()
  routeDaemon((r) => {
    if (r.path.endsWith('/state')) {
      return json({ daemon_seq: 1, projects: [{ ...project(), bots: [bot('b1'), bot(newId)] }], bots: [bot('b1'), bot(newId)], runs: [], turns: [] })
    }
    if (r.path.endsWith('/projects/p1/bots') && r.method === 'POST') return json({ bot_id: newId, name: newId, remapped: null })
    if (r.path.endsWith('/fork') || r.path.endsWith('/promote') || r.path.endsWith('/clone')) return json({ bot_id: newId, name: newId, start_error: null })
    if (r.path.includes('/messages')) return json({ messages: [], turns: [], has_more: false })
    return json({})
  })
}

const seed = (extra: Record<string, unknown> = {}) =>
  useStore.setState({
    bots: [bot('b1')],
    projects: [project()],
    selectedBotId: 'b1',
    selectedProjectId: null,
    shellView: null,
    rightTab: 'chat',
    settingsBotId: null,
    ...extra,
  } as never)

const expectArrivedAt = (id: string) => {
  const s = useStore.getState()
  assert.equal(s.selectedBotId, id, '切到新 bot')
  assert.equal(s.selectedProjectId, null, '離開群組時間軸')
  assert.equal(s.shellView, null, '整頁 shell 收掉')
  assert.equal(s.rightTab, 'chat', '終端分頁回到對話')
  assert.equal(s.settingsBotId, null, '舊 bot 的設定卡不留著')
  assert.ok(requests.some((r) => r.path.includes(`/bots/${id}/messages`)), '新 bot 的對話有載入（selectBot 做的）')
}

test('群組畫面上 addBot：選到新 bot 並離開群組', async () => {
  route('b2')
  seed({ selectedProjectId: 'p1', rightTab: 'terminal', settingsBotId: 'b1' })
  await useStore.getState().addBot('p1', { name: 'x', kind: 'claude', identity: null } as never)
  await settleUntil(messagesRequested('b2'), '新 bot 的對話請求')
  expectArrivedAt('b2')
})

test('整頁 shell 開著時 forkBot：shellView 清掉、切到 fork 出的 bot', async () => {
  route('b2')
  seed({ shellView: shell })
  await useStore.getState().forkBot('b1')
  await settleUntil(messagesRequested('b2'), '新 bot 的對話請求')
  expectArrivedAt('b2')
})

test('終端分頁開著時 cloneBot：新 bot 不會開在終端分頁', async () => {
  route('b2')
  seed({ rightTab: 'terminal' })
  await useStore.getState().cloneBot('b1')
  await settleUntil(messagesRequested('b2'), '新 bot 的對話請求')
  expectArrivedAt('b2')
})

test('設定卡開著時 promoteBot：settingsBotId 清掉、切到升級後的 bot', async () => {
  route('b2')
  seed({ settingsBotId: 'b1', selectedProjectId: 'p1' })
  await useStore.getState().promoteBot('b1')
  await settleUntil(messagesRequested('b2'), '新 bot 的對話請求')
  expectArrivedAt('b2')
})
