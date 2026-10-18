import test from 'node:test'
import assert from 'node:assert/strict'
import { KeyQueue } from '../lib/keyQueue.ts'
import type { Run } from '../api/types.ts'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'

const { useStore } = await import('./store.ts')

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

function seed() {
  reset()
  useStore.setState({ runs: { b1: { id: 'r1' } as Run }, notices: [] })
}

function keyRequests() {
  return requests.filter((request) => request.path.endsWith('/bots/b1/keys'))
}

function queueForBot() {
  const settled: (unknown | null)[] = []
  const waiters: ((error: unknown | null) => void)[] = []
  const queue = new KeyQueue(
    (keys) => useStore.getState().sendKeys('b1', keys),
    (error) => {
      settled.push(error)
      waiters.shift()?.(error)
    },
  )
  return {
    queue,
    settled,
    nextSettled: () => new Promise<unknown | null>((resolve) => waiters.push(resolve)),
  }
}

const keysOf = () => keyRequests().map((request) => (request.body as { keys: string[] }).keys)

test('sendKeys 503：通知一次並 reject，KeyQueue 丟掉排隊的 Enter，後續輸入仍可送', async () => {
  seed()
  let keyPost = 0
  routeDaemon((request) => {
    if (!request.path.endsWith('/bots/b1/keys')) return json({})
    return keyPost++ === 0 ? json({ error: 'upstream', message: 'temporary failure' }, 503) : json({})
  })
  const { queue, settled, nextSettled } = queueForBot()

  const failed = nextSettled()
  queue.push(['down'])
  queue.push(['enter'])
  const error = await failed
  assert.ok(error instanceof Error, 'sendKeys 的拒絕由 KeyQueue 收到')
  assert.deepEqual(keysOf(), [['down']], '第一批失敗後排隊中的 Enter 沒送')
  assert.equal(useStore.getState().notices.length, 1, 'store 只通知一次')

  const recovered = nextSettled()
  queue.push(['up'])
  assert.equal(await recovered, null, '失敗後佇列仍可處理新輸入')
  assert.deepEqual(keysOf(), [['down'], ['up']])
  assert.equal(settled.length, 2)
  assert.equal(useStore.getState().notices.length, 1, '成功重試沒有多餘通知')
})

test('sendKeys 非 run mismatch 的 409：不重試，佇列丟掉 Enter，store 只通知一次', async () => {
  seed()
  routeDaemon((request) =>
    request.path.endsWith('/bots/b1/keys') ? json({ error: 'conflict', reason: 'service_pane' }, 409) : json({}),
  )
  const { queue, nextSettled } = queueForBot()

  const failed = nextSettled()
  queue.push(['down'])
  queue.push(['enter'])
  assert.ok(await failed instanceof Error)
  assert.deepEqual(keysOf(), [['down']], '409 拒絕不會讓 Enter 接著送')
  assert.equal(useStore.getState().notices.length, 1)
})

test('run mismatch 重試仍失敗：只重送原批次，排隊 Enter 丟掉，通知一次', async () => {
  seed()
  let keyPost = 0
  routeDaemon((request) => {
    if (!request.path.endsWith('/bots/b1/keys')) return json({})
    return keyPost++ === 0
      ? json({ error: 'conflict', reason: 'run mismatch', run_id: 'r2' }, 409)
      : json({ error: 'upstream', message: 'retry failed' }, 503)
  })
  const { queue, nextSettled } = queueForBot()

  const failed = nextSettled()
  queue.push(['down'])
  queue.push(['enter'])
  assert.ok(await failed instanceof Error)
  assert.deepEqual(keysOf(), [['down'], ['down']], '用新 run id 重試原批次，不送排隊 Enter')
  assert.deepEqual(keyRequests().map((request) => (request.body as { expect_run_id?: string }).expect_run_id), ['r1', 'r2'])
  assert.equal(useStore.getState().notices.length, 1, '只在最後失敗時通知一次')
})

test('sendKeys 成功：佇列依序送下移與 Enter，沒有失敗通知', async () => {
  seed()
  routeDaemon(() => json({}))
  const { queue, nextSettled } = queueForBot()

  const done = nextSettled()
  queue.push(['down'])
  queue.push(['enter'])
  assert.equal(await done, null)
  assert.deepEqual(keysOf(), [['down'], ['enter']])
  assert.deepEqual(keyRequests().map((request) => (request.body as { expect_run_id?: string }).expect_run_id), ['r1', 'r1'])
  assert.deepEqual(useStore.getState().notices, [])
})
