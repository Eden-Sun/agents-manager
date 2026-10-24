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
  // a fresh call after settle starts a new flight
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
  // 排在迴圈收尾之後、清 inflight 之前：這個呼叫不能被當成「合併進已經不會再跑的那一趟」而吞掉。
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

  // 1. 啟動舊 flight (runs = 1)
  const pOld = refresh()
  assert.equal(runs, 1)

  // 2. 測試環境重置：呼叫 resetForTest()
  refresh.resetForTest?.()

  // 3. 測試發起新的呼叫 (runs 應變成 2)
  const pNew1 = refresh()
  assert.equal(runs, 2, 'reset 後新呼叫應獨立啟動新的 flight，不被舊 inflight 阻塞')

  // 4. 在新 flight 執行期間再呼叫一次 refresh()：應該合併為 trailing 請求
  const pNewTrailing = refresh()
  assert.equal(runs, 2, '新 flight 執行期間進來的呼叫應等 trailing run，此時不開第三次')

  // 5. 舊 flight 此時才 resolve（晚到）
  gateOld.resolve()
  await pOld
  // 驗證：舊 flight 收尾不應該干擾新 flight 的 looping 或提前清掉新 inflight
  assert.equal(runs, 2, '舊 flight 收尾不得把新 flight 提早中斷或多觸發')

  // 6. 新 flight 的第一趟 resolve，應自動觸發 trailing run (runs 變 3)
  gateNew1.resolve()
  await new Promise((r) => setImmediate(r))
  assert.equal(runs, 3, '新 flight 中途進來的請求在第一趟完成後只多跑一趟 trailing')

  // 7. trailing run 完成
  gateNew2.resolve()
  await Promise.all([pNew1, pNewTrailing])
  assert.equal(runs, 3)
})
