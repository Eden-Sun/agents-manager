/**
 * bot 的輸入框卡著草稿（409 `composer_busy`）：網頁要把框裡的字留在輸入列旁邊、讓使用者處理，不是 toast 一閃就沒了
 * （2026-09-26 w16T:p3）。草稿一律是中性假字。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Project } from '../api/types.ts'
import { ApiError } from '../api/types.ts'
import { draftBlockFrom } from './composerDraft.ts'

const { useStore } = await import('./store.ts')

const json = (body: unknown, status: number) => new Response(JSON.stringify(body), { status })

const busy = (draft: string, reason = 'composer_busy') =>
  json({ error: 'conflict', reason, retryable: true, sent: false, run_id: 'r1', draft, draft_truncated: false, draft_token: `token:${draft}`, draft_actions: ['submit', 'clear'] }, 409)
const ok = () => json({ turn_id: 't1', message_id: 'm1', delivery: 'ok' }, 200)
const uncommitted = (sent: boolean | null | undefined) =>
  json({ error: 'delivery_state_uncommitted', turn_id: 't1', sent, delivery: sent === false ? 'failed' : 'pending' }, 503)

function seed() {
  reset()
  useStore.setState({
    projects: [{ id: 'p1', label: 'p', path: '/p', host: 'local' } as Project],
    bots: [{ id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude', identity: null } as Bot],
    notices: [],
    turns: {},
    messages: {},
    composerDrafts: {},
  })
}

const block = () => useStore.getState().composerDrafts.b1
const notices = () => useStore.getState().notices
const lastBody = () => requests.filter((r) => r.path.endsWith('/prompt')).at(-1)?.body as Record<string, unknown>

test('撞到框裡有字：留下一條草稿（框裡的字＋可用的動作），不跳 toast', async () => {
  seed()
  routeDaemon(() => busy('一段留在框裡的假草稿'))
  assert.equal(await useStore.getState().sendPrompt('b1', '我自己要送的'), false)
  assert.deepEqual(block(), { draft: '一段留在框裡的假草稿', token: 'token:一段留在框裡的假草稿', truncated: false, actions: ['submit', 'clear'] })
  assert.deepEqual(notices(), [], '有那一條就不用 toast')
})

test('舊 daemon（409 沒帶 draft）：照舊只跳一句人話，不留一條空的', async () => {
  seed()
  routeDaemon(() => json({ error: 'conflict', reason: 'composer_busy', retryable: true, sent: false }, 409))
  assert.equal(await useStore.getState().sendPrompt('b1', 'x'), false)
  assert.equal(block(), undefined)
  assert.equal(notices().length, 1)
  assert.match(notices()[0].text, /沒送出/)
})

test('清掉再送我這則：帶 clear_draft＋expect_draft_token＋我的字；送出去了那一條就收掉', async () => {
  seed()
  useStore.setState({ composerDrafts: { b1: { draft: '一段留在框裡的假草稿', token: 'token:一段留在框裡的假草稿', truncated: false, actions: ['submit', 'clear'] } } })
  routeDaemon(() => ok())
  assert.equal(await useStore.getState().sendPrompt('b1', '我自己要送的', [], false, false, { action: 'clear', token: 'token:一段留在框裡的假草稿' }), true)
  const body = lastBody()
  assert.equal(body.clear_draft, true)
  assert.equal(body.expect_draft_token, 'token:一段留在框裡的假草稿')
  assert.equal(body.text, '我自己要送的')
  assert.equal(body.submit_draft, undefined)
  assert.equal(block(), undefined)
})

test('送出框裡那段：帶 submit_draft＋expect_draft_token、不帶 text；送出去了那一條就收掉', async () => {
  seed()
  useStore.setState({ composerDrafts: { b1: { draft: '一段留在框裡的假草稿', token: 'token:一段留在框裡的假草稿', truncated: false, actions: ['submit', 'clear'] } } })
  routeDaemon(() => ok())
  assert.equal(await useStore.getState().sendPrompt('b1', '', [], false, false, { action: 'submit', token: 'token:一段留在框裡的假草稿' }), true)
  const body = lastBody()
  assert.equal(body.submit_draft, true)
  assert.equal(body.expect_draft_token, 'token:一段留在框裡的假草稿')
  assert.equal('text' in body, false)
  assert.equal(block(), undefined)
  assert.equal(useStore.getState().turns.b1?.t1?.status, 'in_flight', '開了回合，輸入框跟一般送出一樣鎖上')
})

test('#625：草稿動作遇到送達狀態尚未寫入的 503，所有送達結果都會清 busy', async () => {
  for (const action of ['submit', 'clear'] as const) {
    for (const sent of [false, true, null, undefined] as const) {
      seed()
      useStore.setState({
        composerDrafts: {
          b1: { draft: '終端裡的草稿', token: 'draft-token', truncated: false, actions: ['submit', 'clear'] },
        },
      })
      routeDaemon((r) =>
        r.path.endsWith('/prompt') ? uncommitted(sent) : json({ messages: [], turns: [], has_more: false }, 200),
      )

      const accepted = await useStore.getState().sendPrompt('b1', '', [], false, false, { action, token: 'draft-token' })
      assert.equal(accepted, sent === false ? false : true, `${action}, sent=${String(sent)}: preserve delivery handling`)
      if (sent === false) {
        assert.deepEqual(block(), {
          draft: '終端裡的草稿',
          token: 'draft-token',
          truncated: false,
          actions: ['submit', 'clear'],
          busy: undefined,
        }, `${action}: uncommitted but not sent keeps the draft and unlocks every button`)
      } else {
        assert.equal(block(), undefined, `${action}: sent or unknown must remove the stale draft bar`)
      }
      await new Promise((resolve) => setTimeout(resolve, 0))
    }
  }
})

test('框裡換了字：那一條換成新的草稿、說一聲；框已經空了：收掉那一條', async () => {
  seed()
  useStore.setState({ composerDrafts: { b1: { draft: '一段留在框裡的假草稿', token: 'token:一段留在框裡的假草稿', truncated: false, actions: ['submit', 'clear'] } } })
  routeDaemon(() => busy('別人剛打的另一段', 'draft_changed'))
  assert.equal(await useStore.getState().sendPrompt('b1', '', [], false, false, { action: 'submit', token: 'token:一段留在框裡的假草稿' }), false)
  assert.equal(block()?.draft, '別人剛打的另一段')
  assert.equal(block()?.token, 'token:別人剛打的另一段')
  assert.equal(block()?.busy, undefined, '按鈕解鎖')
  assert.match(notices().at(-1)?.text ?? '', /框裡的字變了/)

  routeDaemon(() => json({ error: 'conflict', reason: 'draft_gone', retryable: false, sent: false, draft: null, draft_actions: [] }, 409))
  assert.equal(await useStore.getState().sendPrompt('b1', '', [], false, false, { action: 'submit', token: 'token:別人剛打的另一段' }), false)
  assert.equal(block(), undefined)
  assert.match(notices().at(-1)?.text ?? '', /已經沒有字/)
})

test('取消只收掉那一條', () => {
  seed()
  useStore.setState({ composerDrafts: { b1: { draft: 'x', token: 'token:x', truncated: false, actions: ['submit'] } } })
  useStore.getState().dismissComposerDraft('b1')
  assert.equal(block(), undefined)
  assert.equal(requests.length, 0, '不送任何東西給 daemon')
})

test('draftBlockFrom：只認 daemon 給的兩種動作；沒有 full-draft token 就不提供動作', () => {
  const e = (body: Record<string, unknown>) => new ApiError(409, { error: 'conflict', ...body }, 'x')
  assert.deepEqual(draftBlockFrom(e({ reason: 'composer_busy', draft: 'd', draft_token: 'token-d', draft_actions: ['submit', 'rm -rf'] }))?.actions, ['submit'])
  assert.deepEqual(draftBlockFrom(e({ reason: 'composer_busy', draft: 'd', draft_actions: ['submit'] }))?.actions, [])
  assert.equal(draftBlockFrom(e({ reason: 'composer_busy', draft: null, draft_actions: [] })), null)
  assert.equal(draftBlockFrom(e({ reason: 'a turn is already in flight', draft: 'd' })), null)
  assert.equal(draftBlockFrom(new Error('x')), null)
})
