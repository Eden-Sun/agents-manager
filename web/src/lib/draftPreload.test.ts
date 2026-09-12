import test from 'node:test'
import assert from 'node:assert/strict'
import { acquirePreload, hasPreload, resetPreloads, restartPreload } from './draftPreload.ts'
import type { Draft } from './choiceDraft.ts'

const draft = { pages: [] } as unknown as Draft
const tick = () => new Promise((r) => setTimeout(r, 1))

/** 記下 start 被叫了幾次；resolve 由測試控制。 */
function starter(result: Draft | null = draft) {
  let calls = 0
  let progress: ((d: number, t: number) => void) | null = null
  let done!: (d: Draft | null) => void
  const job = new Promise<Draft | null>((r) => {
    done = r
  })
  return {
    get calls() {
      return calls
    },
    progress: (d: number, t: number) => progress?.(d, t),
    finish: () => done(result),
    start: (onProgress: (d: number, t: number) => void) => {
      calls += 1
      progress = onProgress
      return job
    },
  }
}

test.beforeEach(() => resetPreloads())

test('兩個視圖同一份問卷：只跑一份預載，兩邊拿同一個結果', async () => {
  const s = starter()
  const a = acquirePreload('b1:q', s.start)
  const b = acquirePreload('b1:q', s.start)
  assert.equal(s.calls, 1)
  assert.equal(a.job, b.job)
  s.finish()
  assert.equal(await b.job, draft)
})

test('進度會廣播給每個持有者；晚接上來的先拿到目前的進度', () => {
  const s = starter()
  const got: string[] = []
  acquirePreload('b1:q', s.start, (d, t) => got.push(`a${d}/${t}`))
  s.progress(1, 3)
  acquirePreload('b1:q', s.start, (d, t) => got.push(`b${d}/${t}`))
  s.progress(2, 3)
  assert.deepEqual(got, ['a1/3', 'b1/3', 'a2/3', 'b2/3'])
})

test('StrictMode 的 mount → cleanup → mount：放掉再馬上接回來不重跑', async () => {
  const s = starter()
  const a = acquirePreload('b1:q', s.start)
  a.release()
  const b = acquirePreload('b1:q', s.start)
  await tick()
  assert.equal(s.calls, 1)
  assert.ok(hasPreload('b1:q'))
  b.release()
  await tick()
  assert.equal(hasPreload('b1:q'), false)
})

test('最後一個視圖放掉之後才丟；一個放掉另一個還在就留著', async () => {
  const s = starter()
  const a = acquirePreload('b1:q', s.start)
  const b = acquirePreload('b1:q', s.start)
  a.release()
  a.release() // 重複放不會扣兩次
  await tick()
  assert.ok(hasPreload('b1:q'))
  b.release()
  await tick()
  assert.equal(hasPreload('b1:q'), false)
})

test('預載失敗（null）不留在表裡，下一次掛上來會重試', async () => {
  const s = starter(null)
  const a = acquirePreload('b1:q', s.start)
  s.finish()
  await a.job
  await tick()
  assert.equal(hasPreload('b1:q'), false)
  const s2 = starter()
  acquirePreload('b1:q', s2.start)
  assert.equal(s2.calls, 1)
})

test('「重新讀取」換一份新的；舊持有者放掉不會把新的丟掉', async () => {
  const s1 = starter()
  const a = acquirePreload('b1:q', s1.start)
  const s2 = starter()
  const b = restartPreload('b1:q', s2.start)
  assert.notEqual(a.job, b.job)
  assert.equal(s2.calls, 1)
  a.release()
  await tick()
  assert.ok(hasPreload('b1:q'))
  // 之後接上來的人拿到新的那份。
  const c = acquirePreload('b1:q', s1.start)
  assert.equal(c.job, b.job)
  assert.equal(s1.calls, 1)
})

test('不同 bot 或不同問卷各跑各的', () => {
  const s = starter()
  acquirePreload('b1:q1', s.start)
  acquirePreload('b1:q2', s.start)
  acquirePreload('b2:q1', s.start)
  assert.equal(s.calls, 3)
})
