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
