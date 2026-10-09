import test from 'node:test'
import assert from 'node:assert/strict'
import type { IdentityStatus } from '../api/types.ts'
import { toIdentityStatusMap } from '../api/normalize.ts'
import { dismissKey, loginPromptText, pendingForPrune, pendingLogins, pruneDismissed, visibleLogins, type LoginPromptInput } from './loginPrompt.ts'

const st = (over: Partial<IdentityStatus> = {}): IdentityStatus => ({
  name: 'cc1',
  kind: 'claude',
  logged_in: true,
  reason: null,
  account: 'fake@example.test',
  plan: null,
  source: 'config',
  config_dir: null,
  login_needed: null,
  ...over,
})

const base = (over: Partial<LoginPromptInput> = {}): LoginPromptInput => ({
  bots: [{ id: 'b1', name: 'ops', project_id: 'p-m4p', identity: 'cc1', kind: 'claude' }],
  projects: [
    { id: 'p-m4p', host: 'm4p' },
    { id: 'p-local', host: 'local' },
  ],
  hosts: [{ name: 'm4p', connected: true, identity_status: { cc1: st() } }],
  localIdentityStatus: {},
  localConnected: true,
  ...over,
})

const down = (over: Partial<IdentityStatus>) => base({ hosts: [{ name: 'm4p', connected: true, identity_status: { cc1: st(over) } }] })

test('normalize：daemon 的 login_needed 讀進來；沒有／形狀不對都是 null（舊 daemon）', () => {
  const m = toIdentityStatusMap({
    a: { name: 'a', kind: 'claude', logged_in: true, login_needed: { since: '2026-10-04T01:00:00Z', via: 'turn_auth_failure' } },
    b: { name: 'b', kind: 'claude', logged_in: true },
    c: { name: 'c', kind: 'claude', login_needed: 'x' },
    d: { name: 'd', kind: 'claude', login_needed: { via: 'turn_auth_failure' } },
  })
  assert.deepEqual(m.a.login_needed, { since: '2026-10-04T01:00:00Z', via: 'turn_auth_failure' })
  for (const k of ['b', 'c', 'd']) assert.equal(m[k].login_needed, null, k)
})

test('已登入、沒有記號：不提示', () => {
  assert.deepEqual(pendingLogins(base()), [])
})

test('探測說未登入、有 bot 綁著：提示（episode＝probe）', () => {
  const p = pendingLogins(down({ logged_in: false }))
  assert.equal(p.length, 1)
  assert.deepEqual({ host: p[0].host, identity: p[0].identity, episode: p[0].episode, via: p[0].via }, { host: 'm4p', identity: 'cc1', episode: 'probe', via: 'probe' })
  assert.deepEqual(p[0].bots, [{ id: 'b1', name: 'ops' }])
})

test('遠端 claude 探測問不出未登入（null），但 daemon 記了回合授權失敗：照樣提示，episode＝since', () => {
  const p = pendingLogins(down({ logged_in: null, login_needed: { since: 'T1', via: 'turn_auth_failure' } }))
  assert.equal(p.length, 1)
  assert.equal(p[0].episode, 'T1')
  assert.equal(p[0].via, 'turn')
})

test('沒有 bot 綁著它（或綁在別台主機）：不提示', () => {
  assert.deepEqual(pendingLogins({ ...down({ logged_in: false }), bots: [] }), [])
  const other = { ...down({ logged_in: false }), bots: [{ id: 'b2', name: 'x', project_id: 'p-local', identity: 'cc1', kind: 'claude' as const }] }
  assert.deepEqual(pendingLogins(other), [], 'cc1 在本機有 bot、但登出的是 m4p 的 cc1')
  const wrongKind = { ...down({ logged_in: false }), bots: [{ id: 'b3', name: 'x', project_id: 'p-m4p', identity: 'cc1', kind: 'codex' as const }] }
  assert.deepEqual(pendingLogins(wrongKind), [])
})

test('只提示 claude 身分；主機連不上就不提示（登入也開不了 pane，離線條已經在講）', () => {
  assert.deepEqual(pendingLogins(down({ logged_in: false, kind: 'codex' })), [])
  assert.deepEqual(pendingLogins({ ...down({ logged_in: false }), hosts: [{ name: 'm4p', connected: false, identity_status: { cc1: st({ logged_in: false }) } }] }), [])
  const local = base({ bots: [{ id: 'b', name: 'l', project_id: 'p-local', identity: 'cc1', kind: 'claude' }], hosts: [], localIdentityStatus: { cc1: st({ logged_in: false }) } })
  assert.equal(pendingLogins(local)[0].host, 'local')
  assert.deepEqual(pendingLogins({ ...local, localConnected: false }), [], '本機與 herdr 斷線')
})

test('關掉只對同一次登出有效；換一次或登入成功後再被登出又會提示', () => {
  const first = pendingLogins(down({ logged_in: null, login_needed: { since: 'T1', via: 'turn_auth_failure' } }))
  const dismissed = { [dismissKey('m4p', 'cc1')]: 'T1' }
  assert.deepEqual(visibleLogins(first, dismissed), [], '同一次：不重複洗版')
  const second = pendingLogins(down({ logged_in: null, login_needed: { since: 'T2', via: 'turn_auth_failure' } }))
  assert.equal(visibleLogins(second, dismissed).length, 1, '新的一次登出：再提示')
  // 登入成功：不需要登入了，關掉的記錄被清掉；之後探測又說未登入（episode probe）就會提示。
  const cleaned = pruneDismissed(dismissed, [])
  assert.deepEqual(cleaned, {})
  assert.equal(visibleLogins(pendingLogins(down({ logged_in: false })), cleaned).length, 1)
  // 沒有要清的就回同一個物件（store 不必重畫）。
  assert.equal(pruneDismissed(dismissed, first), dismissed)
})

test('主機暫時斷線不算登入成功：關掉的紀錄保留，重連後同一次登出不再提示', () => {
  const dismissed = { [dismissKey('m4p', 'cc1')]: 'T1' }
  const offline = base({ hosts: [{ name: 'm4p', connected: false, identity_status: {} }] })
  const kept = pruneDismissed(dismissed, pendingForPrune(offline, pendingLogins(offline), dismissed))
  assert.deepEqual(kept, dismissed, '斷線期間不清掉')
  const back = down({ logged_in: null, login_needed: { since: 'T1', via: 'turn_auth_failure' } })
  assert.deepEqual(visibleLogins(pendingLogins(back), kept), [], '重連後同一次登出：不再洗版')
})

test('本機 WebSocket 斷線同理：本機的關掉紀錄保留', () => {
  const dismissed = { [dismissKey('local', 'cc1')]: 'T1' }
  const offline = base({ localConnected: false, bots: [{ id: 'b', name: 'l', project_id: 'p-local', identity: 'cc1', kind: 'claude' }], hosts: [] })
  assert.deepEqual(pruneDismissed(dismissed, pendingForPrune(offline, pendingLogins(offline), dismissed)), dismissed)
})

test('來源連著、但已不需要登入：關掉的紀錄照樣清掉（登入成功）', () => {
  const dismissed = { [dismissKey('m4p', 'cc1')]: 'T1' }
  const ok = down({ logged_in: true, login_needed: null })
  assert.deepEqual(pruneDismissed(dismissed, pendingForPrune(ok, pendingLogins(ok), dismissed)), {})
})

test('提示文字：說清楚哪個身分、哪台主機、幾顆 bot、怎麼發現的', () => {
  const [turn] = pendingLogins(down({ logged_in: null, login_needed: { since: 'T', via: 'turn_auth_failure' } }))
  assert.equal(loginPromptText(turn).title, 'cc1（m4p）已登出')
  assert.match(loginPromptText(turn).detail, /1 顆 bot.*回合.*失敗/)
  const [probe] = pendingLogins(down({ logged_in: false }))
  assert.match(loginPromptText(probe).detail, /偵測到已登出/)
  const local = base({ bots: [{ id: 'b', name: 'l', project_id: 'p-local', identity: 'cc1', kind: 'claude' }], hosts: [], localIdentityStatus: { cc1: st({ logged_in: false }) } })
  assert.match(loginPromptText(pendingLogins(local)[0]).title, /本機/)
})
