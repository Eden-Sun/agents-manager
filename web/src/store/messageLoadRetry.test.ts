import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'

const { useStore, reloadLoadedConversations } = await import('./store.ts')

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
const msgs = () => json({ messages: [], turns: [], has_more: false })

test('載入失敗會記旗標，重試成功後清掉', async () => {
  reset()
  let fail = true
  routeDaemon((r) => (r.path.includes('/bots/b1/messages') ? (fail ? json({ error: 'boom' }, 500) : msgs()) : json({})))
  await useStore.getState().loadMessages('b1')
  assert.equal(useStore.getState().messageLoadFailed.b1, true, '第一次失敗要記旗標')
  assert.notEqual(useStore.getState().loadedBots.b1, true)
  fail = false
  await useStore.getState().loadMessages('b1')
  assert.equal(useStore.getState().messageLoadFailed.b1, undefined, '重試成功要清掉旗標')
  assert.equal(useStore.getState().loadedBots.b1, true)
})

test('已載入過的對話，重載失敗不記旗標（畫面不換成重試鈕）', async () => {
  reset()
  let fail = false
  routeDaemon((r) => (r.path.includes('/bots/b1/messages') ? (fail ? json({ error: 'boom' }, 500) : msgs()) : json({})))
  await useStore.getState().loadMessages('b1')
  assert.equal(useStore.getState().loadedBots.b1, true)
  fail = true
  await useStore.getState().loadMessages('b1')
  assert.equal(useStore.getState().messageLoadFailed.b1, undefined)
  assert.equal(useStore.getState().loadedBots.b1, true)
})

test('重連補抓包含選取中但沒載成功的那一顆', async () => {
  reset()
  routeDaemon((r) => (r.path.includes('/bots/b1/messages') ? msgs() : json({})))
  useStore.setState({ selectedBotId: 'b1', loadedBots: {} })
  requests.length = 0
  await reloadLoadedConversations(useStore.getState)
  assert.ok(requests.some((r) => r.method === 'GET' && r.path.includes('/bots/b1/messages')), '選取中的 b1 要重抓')
})
