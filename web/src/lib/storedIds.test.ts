import test from 'node:test'
import assert from 'node:assert/strict'
import { pruneStoredIds } from './storedIds.ts'

/** 有 length／key 的假 localStorage（pruneStoredIds 要列舉鍵）。 */
function stubStorage(init: Record<string, string> = {}) {
  const map = new Map<string, string>(Object.entries(init))
  ;(globalThis as unknown as { localStorage: unknown }).localStorage = {
    get length() {
      return map.size
    },
    key: (i: number) => [...map.keys()][i] ?? null,
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
  }
  return map
}

test('已刪除的 bot／專案留下的鍵會清掉，還活著的保留', () => {
  const map = stubStorage({
    'am.collapsedChildren': JSON.stringify(['b1', 'gone']),
    'am.collapsedProjects': JSON.stringify(['p1', 'p-gone']),
    'am.previewCol.openBots': JSON.stringify({ b1: true, gone: true }),
    'am:lastKind:p1': 'claude',
    'am:lastKind:p-gone': 'codex',
    'am.kindDisplay': 'text',
  })
  pruneStoredIds({ botIds: ['b1'], projectIds: ['p1'] })
  assert.deepEqual(JSON.parse(map.get('am.collapsedChildren') ?? '[]'), ['b1'])
  assert.deepEqual(JSON.parse(map.get('am.collapsedProjects') ?? '[]'), ['p1'])
  assert.deepEqual(JSON.parse(map.get('am.previewCol.openBots') ?? '{}'), { b1: true })
  assert.equal(map.get('am:lastKind:p1'), 'claude')
  assert.equal(map.has('am:lastKind:p-gone'), false)
  assert.equal(map.get('am.kindDisplay'), 'text', '不認得的鍵一律不碰')
})

test('儲存讀不到或寫不進去時不丟例外（隱私模式）', () => {
  ;(globalThis as unknown as { localStorage: unknown }).localStorage = {
    get length() {
      throw new Error('SecurityError')
    },
    key: () => {
      throw new Error('SecurityError')
    },
    getItem: () => {
      throw new Error('SecurityError')
    },
    setItem: () => {
      throw new Error('QuotaExceededError')
    },
    removeItem: () => {
      throw new Error('SecurityError')
    },
  }
  assert.doesNotThrow(() => pruneStoredIds({ botIds: [], projectIds: [] }))
})
