import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Project } from '../api/types.ts'

const { useStore } = await import('./store.ts')
const { sendNowButton, sendNowNotice } = await import('./sendNowCopy.ts')
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

const project = { id: 'p1', path: '/p1', label: 'p1', host: 'local' } as Project
const seed = (kind: 'claude' | 'codex') => {
  reset()
  useStore.setState({ bots: [{ id: 'b1', name: 'b1', project_id: 'p1', kind } as Bot], projects: [project] })
}
const noticeTexts = () => useStore.getState().notices.map((n) => n.text)

test('codex steer 成功（send_now: steered）顯示「已插入進行中的回合」，不能說 claude 沒有 send-now 鍵', async () => {
  seed('codex')
  routeDaemon((req) => (req.path.endsWith('/prompt') ? json({ turn_id: 't1', message_id: 'm1', delivery: 'ok', send_now: 'steered' }) : json({})))
  const ok = await useStore.getState().sendPrompt('b1', '改方向', [], true)
  assert.equal(ok, true, JSON.stringify(requests))
  const texts = noticeTexts()
  assert.ok(texts.some((t) => t.includes('已插入進行中的回合')), JSON.stringify(texts))
  assert.ok(!texts.some((t) => t.includes('claude') || t.includes('沒有插隊')), JSON.stringify(texts))
})

test('claude 真的插隊（interrupted）或閒著（idle）不跳任何「沒插隊」說明', async () => {
  for (const sendNow of ['interrupted', 'idle']) {
    seed('claude')
    routeDaemon((req) => (req.path.endsWith('/prompt') ? json({ turn_id: 't1', message_id: 'm1', delivery: 'ok', send_now: sendNow }) : json({})))
    await useStore.getState().sendPrompt('b1', '先看這句', [], true)
    assert.deepEqual(noticeTexts(), [], sendNow)
  }
})

test('閒著的 bot 按插隊被閘門拒絕（200 帶 send_now_*）：依 kind 講原因，codex 不再被說成 claude 版本問題', async () => {
  seed('codex')
  routeDaemon((req) => (req.path.endsWith('/prompt') ? json({ turn_id: 't1', message_id: 'm1', delivery: 'ok', send_now: 'send_now_unsupported_kind' }) : json({})))
  await useStore.getState().sendPrompt('b1', '改方向', [], true)
  const texts = noticeTexts()
  assert.ok(texts.some((t) => t.includes('instant_interrupt')), JSON.stringify(texts))
  assert.ok(!texts.some((t) => t.includes('claude')), JSON.stringify(texts))

  seed('claude')
  routeDaemon((req) => (req.path.endsWith('/prompt') ? json({ turn_id: 't1', message_id: 'm1', delivery: 'ok', send_now: 'send_now_cli_too_old' }) : json({})))
  await useStore.getState().sendPrompt('b1', '先看這句', [], true)
  assert.ok(noticeTexts().some((t) => t.includes('claude') && t.includes('2.1.275')), JSON.stringify(noticeTexts()))
})

test('daemon 409 帶的 send_now_message（旗標關／版本不足）原樣顯示給 codex', async () => {
  seed('codex')
  routeDaemon((req) =>
    req.path.endsWith('/prompt')
      ? json({ error: 'conflict', reason: 'a turn is already in flight', send_now_refused: 'send_now_codex_too_old', send_now_message: '這個 run 跑的 codex 比 0.159.0 舊，沒有 instant_interrupt；重啟套用新版後才能插隊。' }, 409)
      : json({}),
  )
  const ok = await useStore.getState().sendPrompt('b1', '改方向', [], true)
  assert.equal(ok, false)
  assert.ok(noticeTexts().some((t) => t.includes('0.159.0')), JSON.stringify(noticeTexts()))
})

test('按鈕與說明依 bot kind 區分：claude 講 send-now 鍵、codex 講 steer，其他 kind 不畫', () => {
  const claude = sendNowButton('claude', '看這句')
  const codex = sendNowButton('codex', '看這句')
  assert.ok(claude && codex)
  assert.equal(claude.label, '插隊')
  assert.ok(claude.title.includes('2.1.275') && claude.title.includes('打斷'))
  assert.ok(codex.title.includes('0.159') && codex.title.includes('instant_interrupt'))
  assert.ok(!codex.title.includes('打斷目前這一輪'), 'codex 不打斷、也不開新回合')
  assert.notEqual(codex.label, claude.label)
  assert.equal(sendNowButton('grok', 'x'), null)
  assert.equal(sendNowButton('shell', 'x'), null)
  // 長文字截斷在 40 字。
  assert.ok(sendNowButton('claude', 'a'.repeat(60))!.title.endsWith('…'))
})

test('sendNowNotice：不認得的值與 null 不誤報', () => {
  assert.equal(sendNowNotice(null, 'claude'), null)
  assert.equal(sendNowNotice(undefined, 'codex'), null)
  assert.equal(sendNowNotice('interrupted', 'claude'), null)
  assert.equal(sendNowNotice('idle', 'codex'), null)
  assert.equal(sendNowNotice('steered', 'codex')?.level, 'info')
})
