import test from 'node:test'
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  confirmGroupTurn,
  dropLegacyGroupCounts,
  isGroupTurn,
  isGroupTurnCandidate,
  noteGroupPrompt,
  noteGroupPrompts,
  resetGroupTurnsForTest,
} from './groupUnread.ts'

test('只有回群組訊息的那一回合算群組回覆；單獨對話、沒有 turn 的不算', () => {
  resetGroupTurnsForTest()
  noteGroupPrompt({ role: 'user', group_id: 'g1', turn_id: 't-group' })
  noteGroupPrompt({ role: 'user', group_id: null, turn_id: 't-direct' })
  noteGroupPrompt({ role: 'assistant', group_id: 'g1', turn_id: 't-assistant' })
  noteGroupPrompt({ role: 'user', group_id: 'g2', turn_id: null })
  assert.equal(isGroupTurn('t-group'), true)
  assert.equal(isGroupTurn('t-direct'), false)
  assert.equal(isGroupTurn('t-assistant'), false)
  assert.equal(isGroupTurn(null), false)
})

test('載入的群組時間軸也會登記；超過上限丟最舊的', () => {
  resetGroupTurnsForTest()
  noteGroupPrompts(Array.from({ length: 501 }, (_, i) => ({ role: 'user' as const, group_id: 'g', turn_id: `t${String(i).padStart(3, '0')}` })))
  assert.equal(isGroupTurn('t000'), false)
  assert.equal(isGroupTurn('t500'), true)
})

test('候選：client_request_id 以收件 bot 的 id 結尾（只決定要不要抓訊息確認）', () => {
  assert.equal(isGroupTurnCandidate({ client_request_id: 'c-group-4:B1' }, 'B1'), true)
  assert.equal(isGroupTurnCandidate({ client_request_id: 'c-group-4:B2' }, 'B1'), false)
  assert.equal(isGroupTurnCandidate({ client_request_id: 'web-01M1' }, 'B1'), false)
  assert.equal(isGroupTurnCandidate({ client_request_id: 'tools-install:claude:01M1' }, 'B1'), false)
  assert.equal(isGroupTurnCandidate({ client_request_id: ':B1' }, 'B1'), false)
  assert.equal(isGroupTurnCandidate({ client_request_id: null }, 'B1'), false)
  assert.equal(isGroupTurnCandidate(undefined, 'B1'), false)
})

type Pg = Awaited<ReturnType<Parameters<typeof confirmGroupTurn>[3]>>
const um = (id: string, turnId: string, groupId: string | null) => ({ id, role: 'user' as const, turn_id: turnId, group_id: groupId })

test('確認：直接 prompt 冒用同樣結尾不算；非候選不問；同回合同時只問一次、結果記住', async () => {
  resetGroupTurnsForTest()
  const calls: string[][] = []
  const load = async (botId: string, turnId: string): Promise<Pg> => {
    calls.push([botId, turnId])
    return { messages: [um('m1', 't-fake', null)], has_more: false }
  }
  const turnRec = { client_request_id: 'mine:B1' }
  const [a, b] = await Promise.all([confirmGroupTurn('B1', 't-fake', turnRec, load), confirmGroupTurn('B1', 't-fake', turnRec, load)])
  assert.deepEqual([a, b], [false, false])
  assert.equal(await confirmGroupTurn('B1', 't-fake', turnRec, load), false)
  assert.equal(await confirmGroupTurn('B1', 't-web', { client_request_id: 'web-1' }, load), false)
  assert.deepEqual(calls, [['B1', 't-fake']])
})

test('確認：只問那個回合的 user 訊息，多到一頁放不下就續翻到掃完，不會中途放棄', async () => {
  resetGroupTurnsForTest()
  const befores: (string | undefined)[] = []
  const pages: Record<string, Pg> = {
    top: { messages: [um('u3', 't-g', null)], has_more: true },
    u3: { messages: [um('u2', 't-g', null)], has_more: true },
    u2: { messages: [um('u1', 't-g', 'grp')], has_more: true },
  }
  const load = async (_b: string, _t: string, before?: string): Promise<Pg> => {
    befores.push(before)
    return pages[before ?? 'top']
  }
  assert.equal(await confirmGroupTurn('B1', 't-g', { client_request_id: 'grp:B1' }, load), true)
  assert.deepEqual(befores, [undefined, 'u3', 'u2'])

  // 掃完（has_more=false）才算「不是群組」。
  resetGroupTurnsForTest()
  const load2 = async (): Promise<Pg> => ({ messages: [um('u9', 't-g', null)], has_more: false })
  assert.equal(await confirmGroupTurn('B1', 't-g', { client_request_id: 'grp:B1' }, load2), false)
})

test('確認：抓失敗會退避重試；全失敗不記成「不是」，之後還能再確認', async () => {
  resetGroupTurnsForTest()
  let n = 0
  const flaky = async (): Promise<Pg> => {
    n += 1
    if (n <= 3) throw new Error('offline')
    return { messages: [um('m1', 't-g', 'grp')], has_more: false }
  }
  const rec = { client_request_id: 'grp:B1' }
  assert.equal(await confirmGroupTurn('B1', 't-g', rec, flaky, [1, 1]), false)
  assert.equal(n, 3)
  assert.equal(await confirmGroupTurn('B1', 't-g', rec, flaky, [1, 1]), true)
})

function withStorage(store: Record<string, string>, failSet: (key: string) => boolean, run: () => void) {
  const g = globalThis as unknown as Record<string, unknown>
  const prev = g.localStorage
  g.localStorage = {
    getItem: (k: string) => (k in store ? store[k] : null),
    setItem: (k: string, v: string) => {
      if (failSet(k)) throw new Error('QuotaExceededError')
      store[k] = v
    },
  }
  try {
    run()
  } finally {
    g.localStorage = prev
  }
}

test('舊群組數字存不回去就不寫遷移標記，下次開機再清；bot 未讀保留', () => {
  const store: Record<string, string> = { 'am.unread': JSON.stringify({ 'group:P1': 150, 'bot:B1': 3 }) }
  withStorage(store, (k) => k === 'am.unread', () => {
    const out = dropLegacyGroupCounts({ bots: { B1: 3 }, groups: { P1: 150 } })
    assert.deepEqual(out, { bots: { B1: 3 }, groups: {} })
  })
  assert.equal(store['am.groupUnread.v3'], undefined)
  withStorage(store, () => false, () => {
    assert.deepEqual(dropLegacyGroupCounts({ bots: { B1: 3 }, groups: { P1: 150 } }), { bots: { B1: 3 }, groups: {} })
  })
  assert.equal(store['am.groupUnread.v3'], '1')
  assert.deepEqual(JSON.parse(store['am.unread']), { 'bot:B1': 3 })
})

// ── 整合：真的 import store、跑 bootstrap，每次開機一個行程（見 groupUnreadBoot.harness.ts） ──

const HARNESS = join(import.meta.dirname, 'groupUnreadBoot.harness.ts')

function boot(storagePath: string, scenario: unknown): { groupUnread: Record<string, number>; botUnread: Record<string, number>; bootError: string | null; fetches: string[] } {
  const r = spawnSync(process.execPath, [HARNESS], {
    env: { ...process.env, AM_HARNESS_STORAGE: storagePath, AM_HARNESS_SCENARIO: JSON.stringify(scenario) },
    encoding: 'utf8',
    timeout: 20_000,
  })
  assert.equal(r.status, 0, `harness failed: ${r.stderr}`)
  const line = r.stdout.trim().split('\n').pop() ?? ''
  const out = JSON.parse(line) as ReturnType<typeof boot>
  assert.equal(out.bootError, null)
  return out
}

const T0 = '2026-09-15T10:00:00Z'
const turn = (id: string, crid: string | null, status: string) => ({
  id,
  conversation_id: `c-${id}`,
  run_id: null,
  origin: 'web',
  status,
  delivery: 'ok',
  client_request_id: crid,
  created_at: T0,
  completed_at: status === 'in_flight' ? null : '2026-09-15T10:05:00Z',
})
const msg = (id: string, turnId: string, role: string, groupId: string | null = null) => ({
  id,
  conversation_id: 'c',
  turn_id: turnId,
  role,
  content: 'x',
  source: role === 'user' ? 'web' : 'hook',
  group_id: groupId,
  created_at: `2026-09-15T10:0${id.length % 10}:00Z`,
})
const bot = (id: string, inFlight: unknown = null) => ({ id, name: id, kind: 'claude', in_flight_turn: inFlight })

test('整合：群組 prompt 在重整前送出、回覆在重整後到達，停在別專案的單 bot 頁也算；一般回合不算', () => {
  const dir = mkdtempSync(join(tmpdir(), 'am-group-unread-'))
  const storage = join(dir, 'storage.json')
  // 重整前：人停在 P2 的 O1 單 bot 頁，P1 從沒打開過；前一個分頁已經做完群組遷移。
  writeFileSync(storage, JSON.stringify({ 'am.selection': JSON.stringify({ botId: 'O1', projectId: null }), 'am.groupUnread.v3': '1' }))
  const state = {
    daemon_seq: 1,
    projects: [
      // 重整前送出、還在跑的回合：B1 回群組訊息；B2 是直接 prompt 但 client_request_id 冒用了群組的結尾；B3 一般 web prompt。
      {
        id: 'P1',
        path: '/p1',
        bots: [bot('B1', turn('t-g', 'grp-1:B1', 'in_flight')), bot('B2', turn('t-d', 'mine:B2', 'in_flight')), bot('B3', turn('t-w', 'web-3', 'in_flight'))],
      },
      { id: 'P2', path: '/p2', bots: [bot('O1')] },
    ],
  }
  const frames = [
    { seq: 2, type: 'message_added', data: { bot_id: 'B1', message: msg('m1', 't-g', 'assistant') } },
    { seq: 3, type: 'turn_updated', data: { bot_id: 'B1', turn: turn('t-g', 'grp-1:B1', 'completed') } },
    { seq: 4, type: 'message_added', data: { bot_id: 'B2', message: msg('m22', 't-d', 'assistant') } },
    { seq: 5, type: 'turn_updated', data: { bot_id: 'B2', turn: turn('t-d', 'mine:B2', 'completed') } },
    { seq: 6, type: 'turn_updated', data: { bot_id: 'B3', turn: turn('t-w', 'web-3', 'completed') } },
    // 開機後才送的群組回合：看過帶 group_id 的 user 訊息，不必再抓。
    { seq: 7, type: 'turn_updated', data: { bot_id: 'B2', turn: turn('t-g2', 'grp-2:B2', 'in_flight') } },
    { seq: 8, type: 'message_added', data: { bot_id: 'B2', message: msg('m333', 't-g2', 'user', 'grp-2') } },
    { seq: 9, type: 'message_added', data: { bot_id: 'B2', message: msg('m4444', 't-g2', 'assistant') } },
    { seq: 10, type: 'turn_updated', data: { bot_id: 'B2', turn: turn('t-g2', 'grp-2:B2', 'completed') } },
    // 單 bot 頁載入的訊息裡有群組 prompt：turn 記錄沒有候選結尾也認得。
    { seq: 11, type: 'turn_updated', data: { bot_id: 'O1', turn: turn('t-o', null, 'completed') } },
  ]
  const page = (...m: unknown[]) => ({ messages: m, turns: [], has_more: false })
  const messages = {
    O1: page(msg('m5', 't-o', 'user', 'grp-3')),
    // 群組 prompt 後面跟著 250 則 assistant：問的是該回合的 user 訊息，長回合照樣認得。
    B1: page(msg('m0', 't-g', 'user', 'grp-1'), ...Array.from({ length: 250 }, (_, i) => msg(`ma${String(i).padStart(3, '0')}`, 't-g', 'assistant'))),
    B2: page(msg('m00', 't-d', 'user')),
  }
  const out = boot(storage, { state, frames, messages })
  assert.deepEqual(out.groupUnread, { P1: 2, P2: 1 })
  assert.deepEqual(out.botUnread, { B1: 1, B2: 2, B3: 1, O1: 1 })
  // 只有開機載入的單 bot 頁與兩個候選回合抓訊息；一般回合（B3）、已認得的（t-g2）不抓。
  assert.deepEqual([...out.fetches].sort(), ['B1', 'B2', 'O1'])

  // 再重整一次：數字從 localStorage 回來，不重算也不歸零。
  const again = boot(storage, { state: { ...state, projects: state.projects.map((p) => ({ ...p, bots: p.bots.map((b) => ({ ...b, in_flight_turn: null })) })) }, frames: [] })
  assert.deepEqual(again.groupUnread, { P1: 2, P2: 1 })
})

test('整合：舊算法的 99+ 連續兩次開機都不會回來，bot 未讀保留', () => {
  const dir = mkdtempSync(join(tmpdir(), 'am-group-unread-'))
  const storage = join(dir, 'storage.json')
  // 人停在 O1：被選中的 bot 開機會依訊息重算，B1 的數字才看得出有沒有被遷移動到。
  writeFileSync(
    storage,
    JSON.stringify({ 'am.unread': JSON.stringify({ 'group:P1': 150, 'bot:B1': 3 }), 'am.selection': JSON.stringify({ botId: 'O1', projectId: null }) }),
  )
  const scenario = { state: { daemon_seq: 1, projects: [{ id: 'P1', path: '/p1', bots: [bot('B1'), bot('O1')] }] }, frames: [] }

  const first = boot(storage, scenario)
  assert.deepEqual(first.groupUnread, {})
  assert.deepEqual(first.botUnread, { B1: 3 })
  const saved = JSON.parse(readFileSync(storage, 'utf8')) as Record<string, string>
  assert.deepEqual(JSON.parse(saved['am.unread']), { 'bot:B1': 3 })
  assert.equal(saved['am.groupUnread.v3'], '1')

  const second = boot(storage, scenario)
  assert.deepEqual(second.groupUnread, {})
  assert.deepEqual(second.botUnread, { B1: 3 })
})

test('整合：72c332a 已寫過 v2 標記、群組又讀回 99+ 的分頁，升級後照樣清掉', () => {
  const dir = mkdtempSync(join(tmpdir(), 'am-group-unread-'))
  const storage = join(dir, 'storage.json')
  writeFileSync(
    storage,
    JSON.stringify({
      'am.unread': JSON.stringify({ 'group:P1': 150, 'bot:B1': 3 }),
      'am.groupUnread.v2': '1',
      'am.selection': JSON.stringify({ botId: 'O1', projectId: null }),
    }),
  )
  const scenario = { state: { daemon_seq: 1, projects: [{ id: 'P1', path: '/p1', bots: [bot('B1'), bot('O1')] }] }, frames: [] }
  assert.deepEqual(boot(storage, scenario).groupUnread, {})
  const second = boot(storage, scenario)
  assert.deepEqual(second.groupUnread, {})
  assert.deepEqual(second.botUnread, { B1: 3 })
})

/** issue #122：排隊中的 turn（起 bot 中、啟動失敗原因更新）推來的 `turn_updated` 不是「回合完成」——
 *  算進去的話未讀先多一，之後真正完成那一次又被同一個 turn id 去重吃掉。 */
test('整合：queued 的 turn_updated 不算完成，之後真正完成才記一次未讀', () => {
  const dir = mkdtempSync(join(tmpdir(), 'am-group-unread-'))
  const storage = join(dir, 'storage.json')
  writeFileSync(storage, JSON.stringify({ 'am.selection': JSON.stringify({ botId: 'O1', projectId: null }), 'am.groupUnread.v3': '1' }))
  const state = { daemon_seq: 1, projects: [{ id: 'P1', path: '/p1', bots: [bot('B3'), bot('O1')] }] }
  const queuedOnly = [
    { seq: 2, type: 'turn_updated', data: { bot_id: 'B3', turn: turn('t-q', 'web-q', 'queued') } },
    { seq: 3, type: 'turn_updated', data: { bot_id: 'B3', turn: { ...turn('t-q', 'web-q', 'queued'), start_error: '找不到 claude' } } },
  ]
  assert.deepEqual(boot(storage, { state, frames: queuedOnly }).botUnread, {}, '還沒開始的不算完成')
  writeFileSync(storage, JSON.stringify({ 'am.selection': JSON.stringify({ botId: 'O1', projectId: null }), 'am.groupUnread.v3': '1' }))
  const done = [...queuedOnly, { seq: 4, type: 'turn_updated', data: { bot_id: 'B3', turn: turn('t-q', 'web-q', 'completed') } }]
  assert.deepEqual(boot(storage, { state, frames: done }).botUnread, { B3: 1 }, '真正完成那一次照樣記')
})

/** issue #509 的另一半：不只是「不要標成已讀」，shell 蓋著群組時新回合要真的 `+1`。 */
test('整合：人在前景、選著專案，但畫面是 shell 面板——群組回覆照樣記未讀', () => {
  const dir = mkdtempSync(join(tmpdir(), 'am-group-unread-'))
  const storage = join(dir, 'storage.json')
  const seed = (shell: boolean) =>
    writeFileSync(
      storage,
      JSON.stringify({
        // 選著 P1 的群組（`viewPane`／「在這裡開 shell」都不會清掉它）。
        'am.selection': JSON.stringify({ botId: null, projectId: 'P1' }),
        'am.groupUnread.v3': '1',
        ...(shell ? { 'am.shellView': JSON.stringify({ host: 'local', paneId: 'w1:p9', cwd: '/p1' }) } : {}),
      }),
    )
  const state = { daemon_seq: 1, projects: [{ id: 'P1', path: '/p1', bots: [bot('B1')] }] }
  const frames = [
    { seq: 2, type: 'turn_updated', data: { bot_id: 'B1', turn: turn('t-g', 'grp-9:B1', 'in_flight') } },
    { seq: 3, type: 'message_added', data: { bot_id: 'B1', message: msg('mu', 't-g', 'user', 'grp-9') } },
    { seq: 4, type: 'message_added', data: { bot_id: 'B1', message: msg('ma', 't-g', 'assistant') } },
    { seq: 5, type: 'turn_updated', data: { bot_id: 'B1', turn: turn('t-g', 'grp-9:B1', 'completed') } },
  ]

  seed(true)
  const covered = boot(storage, { state, frames, focus: true, shells: [{ pane_id: 'w1:p9' }] })
  assert.deepEqual(covered.groupUnread, { P1: 1 }, 'shell 蓋著時群組時間軸看不到，回覆要算未讀')

  // 對照：同樣在前景、同樣選著 P1，沒有 shell 就是真的在看，標成已讀。
  seed(false)
  assert.deepEqual(boot(storage, { state, frames, focus: true }).groupUnread, {})
})
