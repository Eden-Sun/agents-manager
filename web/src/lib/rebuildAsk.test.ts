import test from 'node:test'
import assert from 'node:assert/strict'
import { ApiError } from '../api/types.ts'
import { classifyRebuildAsk, rebuildAskNotice, rebuildAsker } from './rebuildAsk.ts'

test('AGM 回合進行中的 409 講明是在忙，不是「請再試一次」', () => {
  const busy = classifyRebuildAsk(null, new ApiError(409, { error: 'conflict', reason: 'a turn is already in flight' }, 'x'))
  assert.equal(busy.kind, 'busy')
  assert.match(rebuildAskNotice(busy).text, /回合進行中/)
})

test('delivery failed／unknown 不能說「已請 AGM 開始重建」', () => {
  for (const delivery of ['failed', 'unknown']) {
    const n = rebuildAskNotice(classifyRebuildAsk({ delivery }, null))
    assert.equal(n.level, 'error', delivery)
    assert.doesNotMatch(n.text, /已請 AGM 開始重建/)
  }
  assert.equal(classifyRebuildAsk({ delivery: 'ok' }, null).kind, 'sent')
  assert.equal(classifyRebuildAsk({ delivery: 'unverified' }, null).kind, 'sent')
})

test('沒送出重按沿用同一個 crid；送到了才換新的', async () => {
  let seq = 0
  const ask = rebuildAsker(() => `c${++seq}`)
  const seen: string[] = []
  await ask(async (crid) => {
    seen.push(crid)
    throw new ApiError(409, { reason: 'composer_busy', retryable: true }, 'x')
  })
  await ask(async (crid) => {
    seen.push(crid)
    throw new Error('connection dropped')
  })
  await ask(async (crid) => {
    seen.push(crid)
    return { delivery: 'ok' }
  })
  await ask(async (crid) => {
    seen.push(crid)
    return { delivery: 'ok' }
  })
  assert.deepEqual(seen, ['c1', 'c1', 'c1', 'c2'])
})

test('那一回合已經 failed：換新的 crid，否則重按只會拿回同一個失敗的回合', async () => {
  let seq = 0
  const ask = rebuildAsker(() => `c${++seq}`)
  const seen: string[] = []
  await ask(async (crid) => (seen.push(crid), { delivery: 'failed' }))
  await ask(async (crid) => (seen.push(crid), { delivery: 'ok' }))
  assert.deepEqual(seen, ['c1', 'c2'])
})
