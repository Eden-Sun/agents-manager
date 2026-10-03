/**
 * claude 的「建議下一句」一鍵送出（2026-10-03）：顯示條件（純函式）、`run.prompt_suggestion` 的讀入、
 * 按下去對 daemon 說什麼、失敗時講什麼。建議文字一律是中性假字。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Project, Run } from '../api/types.ts'
import { ApiError } from '../api/types.ts'
import { toRun } from '../api/normalize.ts'
import { suggestionFailureText, visibleSuggestion } from './promptSuggestion.ts'

const { useStore } = await import('./store.ts')

const json = (body: unknown, status: number) => new Response(JSON.stringify(body), { status })
const SUGGESTION = '跑一次完整測試再收尾'

const idleRun = (over: Partial<Run> = {}): Run => ({ id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle', prompt_suggestion: SUGGESTION, ...over }) as Run

const base = { kind: 'claude', run: idleRun(), draft: '', attachments: 0, composerBusy: false, draftBarOpen: false }

test('run.prompt_suggestion 讀進來；沒帶／null／空字串／形狀不對都是 null（舊 daemon 沒這個欄位）', () => {
  const r = { id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle' }
  assert.equal(toRun({ ...r, prompt_suggestion: SUGGESTION })?.prompt_suggestion, SUGGESTION)
  assert.equal(toRun({ ...r })?.prompt_suggestion, null)
  for (const bad of [null, '', 3, [], {}, true]) assert.equal(toRun({ ...r, prompt_suggestion: bad })?.prompt_suggestion, null, JSON.stringify(bad))
})

test('顯示條件：claude、idle、有建議，而且輸入框沒字、沒附件、沒回合在跑／排隊、沒有草稿那一條', () => {
  assert.equal(visibleSuggestion(base), SUGGESTION)
  assert.equal(visibleSuggestion({ ...base, run: idleRun({ prompt_suggestion: `  ${SUGGESTION}  ` }) }), SUGGESTION, '去頭尾空白')
  const hidden: [string, Partial<typeof base>][] = [
    ['輸入框有字', { draft: '我正在打' }],
    ['有附件', { attachments: 1 }],
    ['回合在跑／排隊／輸入框鎖著', { composerBusy: true }],
    ['框裡卡著草稿那一條開著', { draftBarOpen: true }],
    ['不是 claude', { kind: 'codex' }],
    ['沒有 run', { run: undefined }],
    ['回合中', { run: idleRun({ agent_status: 'working' }) }],
    ['blocked', { run: idleRun({ agent_status: 'blocked' }) }],
    ['run 不是 running', { run: idleRun({ state: 'stopped' }) }],
    ['沒有建議', { run: idleRun({ prompt_suggestion: null }) }],
    ['建議是空白', { run: idleRun({ prompt_suggestion: '   ' }) }],
  ]
  for (const [why, over] of hidden) assert.equal(visibleSuggestion({ ...base, ...over }), null, why)
  assert.equal(visibleSuggestion({ ...base, draft: '   \n ' }), SUGGESTION, '輸入框只有空白不算有字')
})

function seed(run: Partial<Run> = {}) {
  reset()
  useStore.setState({
    projects: [{ id: 'p1', label: 'p', path: '/p', host: 'local' } as Project],
    bots: [{ id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude', identity: null } as Bot],
    runs: { b1: idleRun(run) },
    notices: [],
    turns: {},
    messages: {},
    composerDrafts: {},
    busy: {},
  })
}
const lastBody = () => requests.filter((r) => r.path.endsWith('/suggestion/accept')).at(-1)?.body as Record<string, unknown>
const notices = () => useStore.getState().notices

test('按下去：POST /bots/:id/suggestion/accept，帶畫面上那句與 run id、一個 client_request_id；回合照一般送出鎖上輸入框', async () => {
  seed()
  routeDaemon(() => json({ turn_id: 't1', message_id: 'm1', delivery: 'ok' }, 200))
  assert.equal(await useStore.getState().acceptSuggestion('b1'), true)
  const req = requests.find((r) => r.path.endsWith('/suggestion/accept'))!
  assert.equal(req.method, 'POST')
  assert.equal(req.path, '/api/bots/b1/suggestion/accept')
  const body = lastBody()
  assert.equal(body.suggestion, SUGGESTION)
  assert.equal(body.expect_run_id, 'r1')
  assert.equal(typeof body.client_request_id, 'string')
  assert.equal(useStore.getState().turns.b1?.t1?.status, 'in_flight', '開了回合')
  assert.deepEqual(notices(), [])
  assert.equal(useStore.getState().busy['suggest:b1'], undefined, '按鈕解鎖')
})

test('沒有建議就什麼都不送', async () => {
  seed({ prompt_suggestion: null })
  assert.equal(await useStore.getState().acceptSuggestion('b1'), false)
  assert.equal(requests.length, 0)
})

test('連點：送出中第二下不再送', async () => {
  seed()
  let release: () => void = () => {}
  const gate = new Promise<void>((r) => (release = r))
  routeDaemon(async () => {
    await gate
    return json({ turn_id: 't1', message_id: 'm1', delivery: 'ok' }, 200)
  })
  const first = useStore.getState().acceptSuggestion('b1')
  await new Promise((r) => setTimeout(r, 0))
  assert.equal(useStore.getState().busy['suggest:b1'], true, '送出中')
  assert.equal(await useStore.getState().acceptSuggestion('b1'), false, '第二下被擋')
  release()
  assert.equal(await first, true)
  assert.equal(requests.filter((r) => r.path.endsWith('/suggestion/accept')).length, 1)
})

test('連線斷在 daemon 收下之後（沒收到回覆）：再按沿用同一個 client_request_id，daemon 才認得是同一件事', async () => {
  seed()
  routeDaemon(() => {
    throw new TypeError('network down')
  })
  assert.equal(await useStore.getState().acceptSuggestion('b1'), false)
  const first = lastBody().client_request_id
  routeDaemon(() => json({ turn_id: 't1', message_id: 'm1', delivery: 'ok' }, 200))
  assert.equal(await useStore.getState().acceptSuggestion('b1'), true)
  assert.equal(lastBody().client_request_id, first)
})

test('失敗：409 的原因講人話、輸入列解鎖；daemon 明確回了就不沿用舊的 request id', async () => {
  seed()
  routeDaemon(() => json({ error: 'conflict', reason: 'suggestion_changed', retryable: true, sent: false, tab_sent: false, suggestion: '別句' }, 409))
  assert.equal(await useStore.getState().acceptSuggestion('b1'), false)
  const first = lastBody().client_request_id
  assert.match(notices().at(-1)?.text ?? '', /換成別句了/)
  assert.equal(useStore.getState().busy['suggest:b1'], undefined)
  assert.equal(useStore.getState().turns.b1, undefined, '沒有開回合')
  routeDaemon(() => json({ turn_id: 't1', message_id: 'm1', delivery: 'ok' }, 200))
  assert.equal(await useStore.getState().acceptSuggestion('b1'), true)
  assert.notEqual(lastBody().client_request_id, first)
})

test('Tab 之後送不出去：說清楚終端變成什麼樣；框裡是別人的字就留一條草稿讓人處理', async () => {
  seed()
  routeDaemon(() =>
    json({ error: 'conflict', reason: 'draft_changed', retryable: true, sent: false, tab_sent: true, suggestion_restored: true, draft: '別人剛打的字', draft_token: 'tok', draft_actions: ['submit', 'clear'] }, 409),
  )
  assert.equal(await useStore.getState().acceptSuggestion('b1'), false)
  assert.equal(useStore.getState().composerDrafts.b1?.draft, '別人剛打的字')
})

test('suggestionFailureText：Tab 按過與否、有沒有還原，各是哪一句', () => {
  const e = (body: Record<string, unknown>) => new ApiError(409, { error: 'conflict', ...body }, 'x')
  assert.match(suggestionFailureText(e({ reason: 'suggestion_gone', tab_sent: false }), 'fb'), /不在了/)
  assert.doesNotMatch(suggestionFailureText(e({ reason: 'suggestion_gone', tab_sent: false }), 'fb'), /Tab/)
  assert.match(suggestionFailureText(e({ reason: 'tab_not_accepted', tab_sent: true }), 'fb'), /沒有收下/)
  assert.match(suggestionFailureText(e({ reason: 'suggestion_changed', tab_sent: true, suggestion_restored: true }), 'fb'), /已按 Tab，框裡那句也清掉了/)
  assert.match(suggestionFailureText(e({ reason: 'suggestion_changed', tab_sent: true, suggestion_restored: false }), 'fb'), /還留在終端的輸入框/)
  assert.equal(suggestionFailureText(e({ reason: 'something_else' }), 'fb'), 'fb', '沒列到的走一般錯誤文字')
  assert.equal(suggestionFailureText(new Error('x'), 'fb'), 'fb')
})
