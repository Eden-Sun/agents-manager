import test from 'node:test'
import assert from 'node:assert/strict'
import type { Bot, Run } from '../api/types.ts'
import { claudeInstallPlan, codexInstallPlan, mergeUpdateChips, updateBatchCounts } from './updateBatch.ts'

const NOTICE = 'Update installed · Restart to update'

const bot = (id: string, over: Partial<Bot> = {}): Bot =>
  ({ id, name: id, kind: 'claude', managed_by: 'user', ...over }) as Bot

const run = (id: string, over: Partial<Run> = {}): Run =>
  ({ id: `r-${id}`, bot_id: id, state: 'running', agent_status: 'idle', update_notice: NOTICE, ...over }) as Run

const none = () => false

test('閒置且帶著更新的才算「可以重啟」', () => {
  const bots = [bot('a'), bot('b')]
  const runs = { a: run('a'), b: run('b') }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(
    c.ready.map((x) => x.name),
    ['a', 'b'],
  )
  assert.deepEqual(c.busy, [])
})

test('working / blocked 進「在忙」而不是「可以重啟」', () => {
  const bots = [bot('idle'), bot('busy'), bot('asking')]
  const runs = {
    idle: run('idle'),
    busy: run('busy', { agent_status: 'working' }),
    asking: run('asking', { agent_status: 'blocked' }),
  }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(
    c.ready.map((x) => x.name),
    ['idle'],
  )
  assert.deepEqual(
    c.busy.map((x) => [x.name, x.why]),
    [
      ['busy', '正在跑'],
      ['asking', '卡在提問，等人回答'],
    ],
  )
})

test('回合還在飛的即使 idle 也不動；starting / stopping 同理', () => {
  const bots = [bot('mid'), bot('booting')]
  const runs = { mid: run('mid'), booting: run('booting', { state: 'starting' }) }
  const c = updateBatchCounts(bots, runs, (id) => id === 'mid')
  assert.deepEqual(c.ready, [])
  assert.deepEqual(
    c.busy.map((x) => x.why),
    ['還有一回合沒收掉', '還在啟動或關閉中'],
  )
})

test('沒有更新在等、grok、沒在跑的都不進帳', () => {
  const bots = [bot('gk', { kind: 'grok' }), bot('clean'), bot('stopped')]
  const runs = { gk: run('gk'), clean: run('clean', { update_notice: null }) }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(c.ready, [])
  assert.deepEqual(c.busy, [])
})

test('2026-09-22：codex 磁碟已裝好新版跟 claude 一樣可以重啟；還沒裝的算「在忙」不算消失', () => {
  const bots = [bot('cx-ok', { kind: 'codex' }), bot('cx-wait', { kind: 'codex' })]
  const runs = {
    'cx-ok': run('cx-ok', { update_notice: 'codex 有新版 0.154.0（這個 run 跑的是 0.154.0），已安裝，重啟套用' }),
    'cx-wait': run('cx-wait', { update_notice: 'codex 有新版 0.154.0 → 0.155.1，需安裝後重啟' }),
  }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(
    c.ready.map((x) => x.name),
    ['cx-ok'],
  )
  assert.deepEqual(
    c.busy.map((x) => [x.name, x.why]),
    [['cx-wait', '新版還沒裝，要先手動安裝才能套用']],
  )
})

test('子 agent 不由一鍵重啟動（daemon Skip::Child，SPEC §6.5a）：進「在忙」名單、不算「可以重啟」', () => {
  // 2026-09-22 起 daemon 一律跳過子 agent；框裡若還把它算進「重啟 N 顆」，數字跟實際重啟的對不上，
  // 只剩子 agent 時按鈕還亮著、按下去卻是 total 0。
  const bots = [bot('kid', { managed_by: 'child' }), bot('kid2', { parent_bot_id: 'mine' }), bot('mine')]
  const runs = { kid: run('kid'), kid2: run('kid2'), mine: run('mine') }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(
    c.ready.map((x) => x.name),
    ['mine'],
  )
  assert.deepEqual(
    c.busy.map((x) => [x.name, x.why]),
    [
      ['kid', '子 agent，由父 Bot 重開'],
      ['kid2', '子 agent，由父 Bot 重開'],
    ],
  )
})

test('使用者自己的 herdr default session 裡的 bot 不由一鍵重啟動（daemon Skip::DefaultSession，SPEC §6.5.1）', () => {
  const bots = [bot('mine-default', { herdr_session: 'default' }), bot('run-default'), bot('mine')]
  const runs = { 'mine-default': run('mine-default'), 'run-default': run('run-default', { herdr_session: 'default' }), mine: run('mine') }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(
    c.ready.map((x) => x.name),
    ['mine'],
  )
  assert.deepEqual(
    c.busy.map((x) => x.name),
    ['mine-default', 'run-default'],
  )
  assert.match(c.busy[0].why, /default session/)
})

// —— header 的 codex「安裝＋重啟」（SPEC §6.9，cli_update）——

const PENDING = 'codex 有新版 0.155.1 → 0.157.0，需安裝後重啟'
const INSTALLED = 'codex 有新版 0.157.0（這個 run 跑的是 0.155.1），已安裝，重啟套用'

test('需安裝的 codex 在一般 chip 的在忙名單裡帶 install 旗標（旁邊那顆負責），一般的在忙不帶', () => {
  const bots = [bot('cx', { kind: 'codex' }), bot('busy')]
  const runs = { cx: run('cx', { update_notice: PENDING }), busy: run('busy', { agent_status: 'working' }) }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(
    c.busy.map((x) => [x.name, x.install ?? false]),
    [
      ['cx', true],
      ['busy', false],
    ],
  )
})

test('codexInstallPlan：只看第一台有需安裝的主機，列出裝好後會重啟／會跳過的 codex', () => {
  const bots = [
    bot('cx-idle', { kind: 'codex', project_id: 'p-local' }),
    bot('cx-busy', { kind: 'codex', project_id: 'p-local' }),
    bot('cx-done', { kind: 'codex', project_id: 'p-local' }),
    bot('cx-kid', { kind: 'codex', project_id: 'p-local', managed_by: 'child' }),
    bot('cx-far', { kind: 'codex', project_id: 'p-far' }),
    bot('cl', { project_id: 'p-local' }),
  ]
  const runs = {
    'cx-idle': run('cx-idle', { update_notice: PENDING }),
    'cx-busy': run('cx-busy', { update_notice: PENDING, agent_status: 'working' }),
    'cx-done': run('cx-done', { update_notice: INSTALLED }),
    'cx-kid': run('cx-kid', { update_notice: PENDING }),
    'cx-far': run('cx-far', { update_notice: PENDING }),
    cl: run('cl'),
  }
  const hostOf = (b: Bot) => (b.project_id === 'p-far' ? 'far' : 'local')
  const p = codexInstallPlan(bots, runs, none, hostOf)
  assert.ok(p)
  assert.equal(p.host, 'local')
  assert.equal(p.notice, PENDING)
  assert.equal(p.installCount, 3, '那台還寫著需安裝的 codex（含在忙、子 agent）')
  assert.deepEqual(
    p.ready.map((x) => x.name),
    ['cx-idle', 'cx-done'],
    '閒置的都會被 daemon 的 codex 批次重啟，已裝好的也算；別台、claude 不算',
  )
  assert.deepEqual(
    p.busy.map((x) => x.name),
    ['cx-busy', 'cx-kid'],
  )
})

test('codexInstallPlan：沒有需安裝的 codex 就沒有這顆 chip', () => {
  const bots = [bot('cx', { kind: 'codex' }), bot('cl')]
  const runs = { cx: run('cx', { update_notice: INSTALLED }), cl: run('cl') }
  assert.equal(
    codexInstallPlan(bots, runs, none, () => 'local'),
    null,
  )
})

test('手機兩種更新都有時合成一顆；桌機或只有一種時照舊', () => {
  assert.equal(mergeUpdateChips(true, true, true), true)
  assert.equal(mergeUpdateChips(false, true, true), false, '桌機額度列放得下兩顆')
  assert.equal(mergeUpdateChips(true, true, false), false)
  assert.equal(mergeUpdateChips(true, false, true), false)
  assert.equal(mergeUpdateChips(true, false, true, true), true, '兩種安裝提示也要合併')
  assert.equal(mergeUpdateChips(true, false, false, false, true), false, '只有 herdr 一顆不合併')
  assert.equal(mergeUpdateChips(true, true, false, false, true), true, 'herdr 跟重啟並存也合成一顆')
})

test('Claude 尚未安裝的 run 不會被一般重啟納入', () => {
  const bots = [bot('claude-pending')]
  const runs = { 'claude-pending': run('claude-pending', { update_notice: 'claude 需安裝 2.1.284，需安裝後重啟' }) }
  assert.deepEqual(updateBatchCounts(bots, runs, none), {
    ready: [],
    busy: [{ botId: 'claude-pending', name: 'claude-pending', why: '新版還沒裝，要先手動安裝才能套用', install: true }],
  })
})

test('codexInstallPlan：同一台的「需安裝」目標不一樣時，取最新的那一版（跟 daemon 核對的目標一致，#569）', () => {
  const bots = [bot('old', { kind: 'codex' }), bot('new', { kind: 'codex' }), bot('two', { kind: 'codex' })]
  const runs = {
    old: run('old', { update_notice: 'codex 有新版 0.155.1 → 0.156.1，需安裝後重啟' }),
    new: run('new', { update_notice: 'codex 有新版 0.155.1 → 0.157.0，需安裝後重啟' }),
    two: run('two', { update_notice: 'codex 有新版 0.155.1 → 0.156.9，需安裝後重啟' }),
  }
  const p = codexInstallPlan(bots, runs, none, () => 'local')
  assert.equal(p?.notice, runs.new.update_notice)
  assert.equal(p?.installCount, 3)
})

test('claudeInstallPlan：有一台落後時一次涵蓋所有已安裝主機，並綁定同一目標版', () => {
  const item = {
    kind: 'claude',
    latest: '2.1.284',
    target: '2.1.284',
    hasUpdate: true,
    hosts: [
      { host: 'local', installedVersion: '2.1.284', error: null, behind: false },
      { host: 'm4p', installedVersion: '2.1.281', error: null, behind: true },
      { host: 'unreadable', installedVersion: null, error: 'timeout', behind: false },
    ],
    notify: null,
    text: null,
  }
  const p = claudeInstallPlan(item)
  assert.ok(p)
  assert.equal(p.target, '2.1.284')
  assert.deepEqual(p.hosts.map((h) => h.host), ['local', 'm4p', 'unreadable'])
  assert.deepEqual(p.installHosts, ['m4p', 'unreadable'])
})

test('codexInstallPlan：沒有 codex 在跑也照上游快照出安裝鈕（本機優先，通知照 daemon 格式）', () => {
  const upstream = {
    kind: 'codex',
    latest: '0.159.0',
    target: '0.159.0',
    hasUpdate: true,
    hosts: [
      { host: 'm4p', installedVersion: '0.157.1', error: null, behind: true },
      { host: 'local', installedVersion: '0.158.0', error: null, behind: true },
    ],
    notify: null,
    text: null,
  }
  const bots = [bot('cl', { kind: 'claude' })]
  const runs = { cl: run('cl', { update_notice: 'claude 有新版，需安裝' }) }
  const p = codexInstallPlan(bots, runs, none, () => 'local', upstream)
  assert.equal(p?.host, 'local', '本機優先')
  assert.equal(p?.notice, 'codex 有新版 0.158.0 → 0.159.0，需安裝後重啟')
  assert.equal(p?.installCount, 0)
  assert.equal(codexInstallPlan(bots, runs, none, () => 'local', { ...upstream, hasUpdate: false }), null)
  assert.equal(codexInstallPlan(bots, runs, none, () => 'local'), null, "claude 的「需安裝」不算 codex 的")
})

test('grok 徽章也算一種更新 chip：手機單獨出現照舊一顆，跟別種並存就合成一顆（#761 補手機合併選單）', () => {
  assert.equal(mergeUpdateChips(true, false, false, false, false, true), false, '只有 grok 一顆不合併（跟 herdr 一樣）')
  assert.equal(mergeUpdateChips(true, true, false, false, false, true), true, 'grok 跟重啟並存要合成一顆')
  assert.equal(mergeUpdateChips(true, false, false, false, true, true), true, 'grok 跟 herdr 並存要合成一顆')
  assert.equal(mergeUpdateChips(false, true, true, true, true, true), false, '桌機照舊並排')
})
