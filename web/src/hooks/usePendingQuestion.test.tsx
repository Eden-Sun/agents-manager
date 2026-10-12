/** `usePendingQuestion`：分頁在背景時不讀 transcript 的那一題，回前景立刻補一次（#909）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useEffect, useState } from 'react'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualClockTimers } from '../testing/manualClockTimers'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot } from '../api/types'
import { usePendingQuestion } from './usePendingQuestion'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

function setVisibility(state: 'hidden' | 'visible') {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => state })
}

/** transport 在真的 fetch 之前還有幾個 await：多讓幾輪 microtask。 */
const flush = () =>
  act(async () => {
    for (let i = 0; i < 20; i++) await Promise.resolve()
  })

function Probe({ botId }: { botId: string }) {
  const { questions, status } = usePendingQuestion(botId)
  return <output data-status={status}>{JSON.stringify(questions)}</output>
}

test('背景時不打 pending-question，回前景立刻補一次', async () => {
  resetStoreForTest()
  useStore.setState({ bots: [{ id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude' } as Bot] })
  let reads = 0
  const realFetch = globalThis.fetch
  globalThis.fetch = (async (input: string) => {
    if (String(input).includes('pending-question')) reads += 1
    return new Response(JSON.stringify({ questions: null }), { status: 200 })
  }) as unknown as typeof fetch
  setVisibility('visible')
  const timers = installManualClockTimers()
  try {
    await mount(<Probe botId="b1" />)
    await flush()
    assert.equal(reads, 1, '掛上去先讀一次')

    setVisibility('hidden')
    await act(async () => timers.clock.advance(5000))
    await flush()
    assert.equal(reads, 1, '背景：5 秒一輪也不打 API')

    setVisibility('visible')
    await act(async () => {
      document.dispatchEvent(new Event('visibilitychange'))
    })
    await flush()
    assert.equal(reads, 2, '回前景：立刻補一次')
  } finally {
    timers.restore()
    setVisibility('visible')
    globalThis.fetch = realFetch
  }
})

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (reason: unknown) => void
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
const question = (name: string) => ({ questions: [{ question: name, options: [{ label: 'yes' }] }],
  jev_options: { questions: [{ index: 0, suggested: 0, scores: [0.9] }] } })
async function fixture(run: (f: {
  reads: ReturnType<typeof deferred<Response>>[]
  switchBot: (id: string) => Promise<void>
  tick: () => Promise<void>
  visible: () => Promise<void>
  answer: (index: number, body: unknown) => Promise<void>
  status: () => string
  shown: () => { question: string; jev: unknown }[]
}) => Promise<void>) {
  resetStoreForTest()
  useStore.setState({ bots: [{ id: 'b1', kind: 'claude' } as Bot, { id: 'b2', kind: 'claude' } as Bot] })
  const reads: ReturnType<typeof deferred<Response>>[] = []
  const realFetch = globalThis.fetch
  globalThis.fetch = (async (input: string) => {
    if (!String(input).includes('pending-question')) return new Response('{}')
    const read = deferred<Response>()
    reads.push(read)
    return read.promise
  }) as unknown as typeof fetch
  setVisibility('visible')
  const timers = installManualClockTimers()
  try {
    let changeBot!: (id: string) => void
    function Switcher() {
      const [id, setId] = useState('b1')
      useEffect(() => { changeBot = setId }, [])
      return <Probe botId={id} />
    }
    const host = await mount(<Switcher />)
    await flush()
    await run({ reads,
      switchBot: async (id) => { await act(async () => changeBot(id)); await flush() },
      tick: async () => { await act(async () => timers.clock.advance(5000)); await flush() },
      visible: async () => {
        setVisibility('hidden')
        await act(async () => document.dispatchEvent(new Event('visibilitychange')))
        setVisibility('visible')
        await act(async () => document.dispatchEvent(new Event('visibilitychange')))
        await flush()
      },
      answer: async (index, body) => { await act(async () => reads[index].resolve(new Response(JSON.stringify(body)))); await flush() },
      status: () => host.querySelector('output')!.getAttribute('data-status')!,
      shown: () => JSON.parse(host.querySelector('output')!.textContent!),
    })
  } finally {
    timers.restore()
    globalThis.fetch = realFetch
    setVisibility('visible')
  }
}
for (const latest of [question('B'), { questions: null }]) {
  test(`新回應 ${latest.questions === null ? 'null' : 'B'} 不被晚到的 A 覆蓋`, async () => {
    await fixture(async ({ reads, tick, answer, shown }) => {
      await tick()
      assert.equal(reads.length, 2)
      await answer(1, latest)
      const expected = shown()
      await answer(0, question('A'))
      assert.deepEqual(shown(), expected)
      assert.deepEqual(shown().map(q => q.question), latest.questions === null ? [] : ['B'])
    })
  })
}
test('慢請求在後續輪詢尚未完成時仍可更新；回前景的新結果優先', async () => {
  await fixture(async ({ tick, visible, answer, shown }) => {
    await tick()
    await tick()
    await answer(0, question('A'))
    assert.equal(shown()[0].question, 'A')
    await visible()
    await answer(3, question('B'))
    await answer(2, question('old'))
    await answer(1, question('older'))
    assert.equal(shown()[0].question, 'B')
  })
})

test('bot 切換立即移除上一顆題目，晚到回應不得跨 bot 套用', async () => {
  await fixture(async ({ tick, switchBot, answer, shown }) => {
    await answer(0, question('A'))
    await tick()
    await switchBot('b2')
    assert.deepEqual(shown(), [])
    await answer(2, question('B'))
    await answer(1, question('old A'))
    assert.equal(shown()[0].question, 'B')
  })
})

for (const error of [new Error('network down'), new DOMException('aborted', 'AbortError')]) {
  test(`失敗 ${error.name} 同時移除舊題與 Jev 建議，回前景取得新題`, async () => {
    await fixture(async ({ reads, tick, visible, answer, shown, status }) => {
      await answer(0, question('A'))
      assert.deepEqual(shown()[0].jev, { option: 0, label: 'yes', score: 0.9 })
      await tick()
      await act(async () => reads[1].reject(error))
      await flush()
      assert.deepEqual(shown(), [])
      assert.equal(status(), 'unavailable')
      await visible()
      await answer(2, question('B'))
      assert.equal(shown()[0].question, 'B')
      assert.equal(status(), 'ready')
    })
  })
}

test('HTTP 500 不留下題目；較新的失敗也攔住較早成功回應', async () => {
  await fixture(async ({ reads, tick, answer, shown }) => {
    await answer(0, question('A'))
    await tick()
    await tick()
    await act(async () => reads[2].resolve(new Response('{}', { status: 500 })))
    await flush()
    assert.deepEqual(shown(), [])
    await answer(1, question('old A'))
    assert.deepEqual(shown(), [])
  })
})

test('晚到的失敗不得清掉較新的成功題目', async () => {
  await fixture(async ({ reads, tick, answer, shown }) => {
    await tick()
    await answer(1, question('B'))
    await act(async () => reads[0].reject(new Error('old failure')))
    await flush()
    assert.equal(shown()[0].question, 'B')
  })
})

test('成功回 null 與讀取失敗有不同狀態', async () => {
  await fixture(async ({ reads, tick, answer, shown, status }) => {
    assert.equal(status(), 'loading')
    await answer(0, { questions: null })
    assert.deepEqual(shown(), [])
    assert.equal(status(), 'ready')
    await tick()
    await act(async () => reads[1].reject(new Error('offline')))
    await flush()
    assert.deepEqual(shown(), [])
    assert.equal(status(), 'unavailable')
  })
})
