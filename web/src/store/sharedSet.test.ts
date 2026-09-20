import test from 'node:test'
import assert from 'node:assert/strict'
import { persistSetDiff } from './sharedSet'

const mem: Record<string, string> = {}
;(globalThis as unknown as { localStorage: unknown }).localStorage = {
  getItem: (k: string) => mem[k] ?? null,
  setItem: (k: string, v: string) => void (mem[k] = v),
}

test('別的分頁剛收合的專案不會被這個分頁的切換洗掉', () => {
  mem.k = JSON.stringify(['pB'])
  persistSetDiff('k', new Set(), new Set(['pA']))
  assert.deepEqual(new Set(JSON.parse(mem.k)), new Set(['pA', 'pB']))
  persistSetDiff('k', new Set(['pA']), new Set())
  assert.deepEqual(JSON.parse(mem.k), ['pB'])
})

test('磁碟上的值壞掉：當空的，不丟例外', () => {
  mem.k = '{oops'
  persistSetDiff('k', new Set(), new Set(['p1']))
  assert.deepEqual(JSON.parse(mem.k), ['p1'])
})
