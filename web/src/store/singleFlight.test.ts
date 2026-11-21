import test from 'node:test'
import assert from 'node:assert/strict'
import { acceptStateSeq, singleFlight } from './singleFlight.ts'

function deferred() {
  let resolve!: () => void
  let reject!: (e: unknown) => void
  const promise = new Promise<void>((res, rej) => { resolve = res; reject = rej })
  return { promise, resolve, reject }
}

test('singleFlight: N concurrent calls share one run plus one trailing run', async () => {
  const gates: ReturnType<typeof deferred>[] = []
  let runs = 0
  const refresh = singleFlight(() => {
    runs++
    const g = deferred()
    gates.push(g)
    return g.promise
  })
  const p1 = refresh()
  const p2 = refresh()
  const p3 = refresh()
  const p4 = refresh()
  assert.equal(runs, 1)
  assert.equal(p1, p2)
  assert.equal(p2, p3)
  gates[0].resolve()
  await new Promise((r) => setImmediate(r))
  assert.equal(runs, 2, 'exactly one trailing run, not one per caller')
  gates[1].resolve()
  await Promise.all([p1, p2, p3, p4])
  assert.equal(runs, 2)
  const p5 = refresh()
  assert.equal(runs, 3)
  gates[2].resolve()
  await p5
})

test('singleFlight: a call after the trailing run starts (and no re-request) does not loop', async () => {
  let runs = 0
  const refresh = singleFlight(async () => { runs++ })
  await refresh()
  await refresh()
  assert.equal(runs, 2)
})

test('singleFlight: a rejected run clears inflight so the next call runs again', async () => {
  let n = 0
  const refresh = singleFlight(async () => {
    n++
    if (n === 1) throw new Error('boom')
  })
  await assert.rejects(refresh(), /boom/)
  await refresh()
  assert.equal(n, 2)
})

test('acceptStateSeq drops older snapshots and keeps newer / equal ones', () => {
  assert.equal(acceptStateSeq(10, 12), 12)
  assert.equal(acceptStateSeq(10, 10), 10)
  assert.equal(acceptStateSeq(10, 9), null)
  assert.equal(acceptStateSeq(10, Number.NaN), 10)
})

test('singleFlight: a call made after the last loop check but before the flight is cleared still starts a run', async () => {
  let runs = 0
  const refresh = singleFlight(async () => { runs++ })
  const first = refresh()
  queueMicrotask(() => { void refresh() })
  await first
  await new Promise((r) => setImmediate(r))
  assert.equal(runs, 2)
})

test('singleFlight: resetForTest 後舊 inflight 才 resolve，新呼叫只跑一次且 again 仍具 trailing 語意', async () => {
  const gateOld = deferred()
  const gateNew1 = deferred()
  const gateNew2 = deferred()
  let runs = 0

  const refresh = singleFlight(async () => {
    runs++
    if (runs === 1) await gateOld.promise
    else if (runs === 2) await gateNew1.promise
    else if (runs === 3) await gateNew2.promise
  })

  const pOld = refresh()
  assert.equal(runs, 1)

  refresh.resetForTest?.()

  const pNew1 = refresh()
  assert.equal(runs, 2, 'reset 後新呼叫應獨立啟動新的 flight，不被舊 inflight 阻塞')

  const pNewTrailing = refresh()
  assert.equal(runs, 2, '新 flight 執行期間進來的呼叫應等 trailing run，此時不開第三次')

  gateOld.resolve()
  await pOld
  assert.equal(runs, 2, '舊 flight 收尾不得把新 flight 提早中斷或多觸發')

  gateNew1.resolve()
  await new Promise((r) => setImmediate(r))
  assert.equal(runs, 3, '新 flight 中途進來的請求在第一趟完成後只多跑一趟 trailing')

  gateNew2.resolve()
  await Promise.all([pNew1, pNewTrailing])
  assert.equal(runs, 3)
})
