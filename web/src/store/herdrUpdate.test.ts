import test from 'node:test'
import assert from 'node:assert/strict'
import type { Bot, Run } from '../api/types.ts'
import type { UpstreamItem } from './upstreamUpdate.ts'
import {
  herdrDoneMessage,
  herdrMenuItem,
  herdrProgress,
  herdrUpdatePlan,
  onHerdrFrame,
  parseHerdrDone,
  reconcileHerdr,
  useHerdrUpdate,
  type HerdrUpdateActive,
} from './herdrUpdate.ts'

const base = { update_id: 'h1', host: 'local', target_version: '0.9.3' }

test('進度照階段走；看不懂的階段、別的 id 都不動', () => {
  let cur: HerdrUpdateActive | null = { id: '', host: 'local', target: '0.9.3', phase: 'starting', willResume: [{ bot_id: 'b1', name: 'am' }], childrenLost: [] }
  cur = herdrProgress(cur, { ...base, phase: 'downloading' })
  assert.deepEqual([cur?.id, cur?.phase, cur?.willResume.length], ['h1', 'downloading', 1])
  assert.equal(herdrProgress(cur, { ...base, phase: '???' }), cur)
  assert.equal(herdrProgress(cur, { ...base, update_id: 'h0', phase: 'restarting' }), cur)
  // 同一階段重送不換物件（selector 不重畫）。
  assert.equal(herdrProgress(cur, { ...base, phase: 'downloading' }), cur)
  // 別的分頁按的：手上沒有也收。
  assert.equal(herdrProgress(null, { ...base, phase: 'waiting_idle' })?.phase, 'waiting_idle')
  assert.equal(herdrProgress(cur, { ...base, phase: 'rolling_back' })?.phase, 'rolling_back')
})

test('done 解析名單，訊息講接回幾顆、沒接回、子 agent', () => {
  const r = parseHerdrDone({
    ...base, ok: true, from: '0.9.1', to: '0.9.3',
    resumed: [{ bot_id: 'b1', name: 'am', run_id: 'r1' }, { bot_id: 'b2', name: 'ag', run_id: 'r2' }],
    failed: [{ bot_id: 'b3', name: 'gx', error: 'boom' }],
    children_lost: [{ bot_id: 'c1', name: 'am-x', parent_bot_id: 'b1' }],
  })
  assert.deepEqual([r.ok, r.resumed.length, r.failed[0].error, r.childrenLost[0].parent_bot_id], [true, 2, 'boom', 'b1'])
  assert.match(herdrDoneMessage(r), /0\.9\.1 → 0\.9\.3；接回 2 顆，1 顆沒接回；1 個子 agent 已結束/)
  const bad = parseHerdrDone({ ...base, ok: false, reason: 'busy_timeout' })
  assert.match(herdrDoneMessage(bad), /沒有升級（等了 30 分鐘還有 Bot 在忙，什麼都沒動）/)
  assert.match(herdrDoneMessage(parseHerdrDone({ ...base, ok: false, reason: 'weird', detail: 'x' })), /（weird）：x/)
  // daemon 失敗時的說明在 `detail`（API.md §12.7b），`restart_failed` 照樣帶接回名單。
  const rolled = parseHerdrDone({ ...base, ok: false, reason: 'restart_failed', detail: '2 分鐘沒回來', resumed: [{ bot_id: 'b1', name: 'am', run_id: 'r1' }] })
  assert.match(herdrDoneMessage(rolled), /已換回舊版）：2 分鐘沒回來；接回 1 顆/)
})

test('快照對帳：undefined 不動、有一筆就採用、沒了就清掉，starting 不清', () => {
  const cur: HerdrUpdateActive = { id: 'h1', host: 'local', target: '0.9.3', phase: 'downloading', willResume: [{ bot_id: 'b1', name: 'am' }], childrenLost: [] }
  assert.equal(reconcileHerdr(cur, undefined), cur)
  assert.equal(reconcileHerdr(cur, []), null)
  const row = { update_id: 'h1', host: 'local', target_version: '0.9.3', phase: 'restarting', started_at: '' }
  const next = reconcileHerdr(cur, [row])
  assert.deepEqual([next?.phase, next?.willResume.length], ['restarting', 1])
  assert.equal(reconcileHerdr(null, [row])?.id, 'h1', '重整後接得回進度')
  const starting: HerdrUpdateActive = { ...cur, id: '', phase: 'starting' }
  assert.equal(reconcileHerdr(starting, []), starting)
})

const item = (hosts: UpstreamItem['hosts']): UpstreamItem => ({ kind: 'herdr', latest: '0.9.3', target: '0.9.3', hasUpdate: true, hosts, notify: null, text: null })
const bot = (id: string, project_id: string, parent_bot_id: string | null = null) => ({ id, name: id, project_id, parent_bot_id }) as Bot
const run = { state: 'running' } as Run

test('計畫：本機落後才可按；遠端、shared 停用並講原因；名單只算那台在跑的', () => {
  const bots = [bot('top', 'p-local'), bot('kid', 'p-local', 'top'), bot('idle', 'p-local'), bot('far', 'p-m4p')]
  const runs = { top: run, kid: run, idle: null, far: run }
  const hostOf = (b: Bot) => (b.project_id === 'p-m4p' ? 'm4p' : 'local')
  const both = item([
    { host: 'local', installedVersion: '0.9.1', error: null, behind: true },
    { host: 'm4p', installedVersion: '0.9.1', error: null, behind: true },
  ])
  const p = herdrUpdatePlan(both, new Set(['m4p']), bots, runs, hostOf)!
  assert.deepEqual([p.host, p.from, p.blockedWhy], ['local', '0.9.1', null])
  assert.deepEqual(p.willResume.map((b) => b.name), ['top'])
  assert.deepEqual(p.childrenLost, [{ botId: 'kid', name: 'kid', parentName: 'top' }])
  assert.match(p.hosts[1].blocked ?? '', /^共用 session/)

  const remoteOnly = herdrUpdatePlan(
    item([
      { host: 'local', installedVersion: '0.9.3', error: null, behind: false },
      { host: 'buildbox', installedVersion: '0.9.1', error: null, behind: true },
    ]),
    new Set(),
    bots,
    runs,
    hostOf,
  )!
  assert.equal(remoteOnly.host, null)
  assert.match(remoteOnly.blockedWhy ?? '', /^遠端主機/)
  assert.deepEqual(remoteOnly.willResume, [])

  assert.equal(herdrUpdatePlan(item([{ host: 'local', installedVersion: '0.9.3', error: null, behind: false }]), new Set(), bots, runs, hostOf), null)
  assert.equal(herdrUpdatePlan({ ...both, kind: 'codex' }, new Set(), bots, runs, hostOf), null)
  assert.equal(herdrUpdatePlan(undefined, new Set(), bots, runs, hostOf), null)
})

test('WS 幀走到 store：progress 更新、done 清掉進度並留結果', () => {
  useHerdrUpdate.setState({ active: null, result: null })
  assert.equal(onHerdrFrame('herdr_update_progress', { ...base, phase: 'stopping' }), null)
  assert.equal(useHerdrUpdate.getState().active?.phase, 'stopping')
  assert.match(herdrMenuItem(useHerdrUpdate.getState().active, null, null) ?? '', /停下 herdr/)
  const n = onHerdrFrame('herdr_update_done', { ...base, ok: true, from: '0.9.1', to: '0.9.3', resumed: [], failed: [], children_lost: [] })
  assert.equal(n?.kind, 'info')
  assert.equal(useHerdrUpdate.getState().active, null)
  assert.equal(useHerdrUpdate.getState().result?.to, '0.9.3')
  // 別的 id 的 done 不清掉手上的進度。
  useHerdrUpdate.setState({ active: { id: 'h2', host: 'local', target: '0.9.3', phase: 'resuming', willResume: [], childrenLost: [] } })
  onHerdrFrame('herdr_update_done', { ...base, ok: false, reason: 'restart_failed' })
  assert.equal(useHerdrUpdate.getState().active?.id, 'h2')
  useHerdrUpdate.setState({ active: null, result: null })
})
