import { test } from 'node:test'
import assert from 'node:assert/strict'
import { KeyQueue } from './keyQueue'

/** 解得開的 promise，用來把「請求還在路上」這段時間握在測試手裡。 */
function deferred<T = void>() {
  let resolve!: (v: T) => void
  let reject!: (e: unknown) => void
  const promise = new Promise<T>((res, rej) => {
    resolve = res
    reject = rej
  })
  return { promise, resolve, reject }
}

test('同一時間只有一個請求在路上，飛的期間按的鍵合成下一批', async () => {
  const sent: string[][] = []
  const gate = [deferred(), deferred()]
  let call = 0
  const q = new KeyQueue(async (keys) => {
    sent.push(keys)
    await gate[call++].promise
  })

  q.push(['l'])
  q.push(['s'])
  q.push(['enter'])
  assert.deepEqual(sent, [['l']], '第一批送出去之前不會有第二個請求')

  gate[0].resolve()
  await new Promise((r) => setTimeout(r, 0))
  assert.deepEqual(sent, [['l'], ['s', 'enter']], '飛的期間累積的鍵一次送，順序照按的順序')

  gate[1].resolve()
  await new Promise((r) => setTimeout(r, 0))
  assert.equal(q.inFlight, false)
})

test('送完才回報，錯誤帶得出來而且不會卡住之後的輸入', async () => {
  const settled: (unknown | null)[] = []
  let fail = true
  const q = new KeyQueue(
    async () => {
      if (fail) throw new Error('boom')
    },
    (e) => settled.push(e),
  )

  q.push(['a'])
  await new Promise((r) => setTimeout(r, 0))
  assert.equal(settled.length, 1)
  assert.equal((settled[0] as Error).message, 'boom')

  fail = false
  q.push(['b'])
  await new Promise((r) => setTimeout(r, 0))
  assert.deepEqual(settled[1], null, '上一批失敗不會讓佇列從此不動')
})

test('一批失敗就丟掉後面排著的鍵，不要接在一段沒進去的輸入後面', async () => {
  const sent: string[][] = []
  const gate = deferred()
  const q = new KeyQueue(async (keys) => {
    sent.push(keys)
    await gate.promise
    throw new Error('nope')
  })

  q.push(['r', 'm'])
  q.push([' ', '-', 'r', 'f'])
  gate.resolve()
  await new Promise((r) => setTimeout(r, 0))
  assert.deepEqual(sent, [['r', 'm']], '第一批失敗之後不再送第二批')
})

test('換 pane 時清掉還沒送的鍵', () => {
  const sent: string[][] = []
  const q = new KeyQueue(async (keys) => {
    sent.push(keys)
    await new Promise((r) => setTimeout(r, 5))
  })
  q.push(['a'])
  q.push(['b'])
  q.clear()
  assert.deepEqual(sent, [['a']], '已經在路上的那批送完，還沒送的不送')
})

test('貼上走同一個佇列：打一半貼一段，順序不會反過來', async () => {
  const log: string[] = []
  const q = new KeyQueue(
    async (keys) => {
      log.push(`keys:${keys.join(',')}`)
      await new Promise((r) => setTimeout(r, 1))
    },
    undefined,
    async (text) => {
      log.push(`text:${text}`)
      await new Promise((r) => setTimeout(r, 1))
    },
  )
  q.push(['e', 'c', 'h', 'o', 'space'])
  q.pushText('hello world')
  q.push(['enter'])
  await new Promise((r) => setTimeout(r, 30))
  assert.deepEqual(log, ['keys:e,c,h,o,space', 'text:hello world', 'keys:enter'])
})

test('沒給 sendText 時貼上直接丟掉，不會假裝送出去', async () => {
  const log: string[] = []
  const q = new KeyQueue(async (keys) => {
    log.push(keys.join(','))
  })
  q.pushText('rm -rf /')
  q.push(['a'])
  await new Promise((r) => setTimeout(r, 10))
  assert.deepEqual(log, ['a'])
})
