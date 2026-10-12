import test from 'node:test'
import assert from 'node:assert/strict'
import { readTabValue, writeTabValue } from './tabStorage.ts'

/** 假的 Storage：`broken` 為真時讀寫都丟例外（隱私模式）。 */
function fakeStore(init: Record<string, string> = {}, broken = false) {
  const map = new Map<string, string>(Object.entries(init))
  return {
    map,
    getItem: (k: string) => {
      if (broken) throw new Error('SecurityError')
      return map.get(k) ?? null
    },
    setItem: (k: string, v: string) => {
      if (broken) throw new Error('QuotaExceededError')
      map.set(k, v)
    },
    removeItem: (k: string) => {
      if (broken) throw new Error('SecurityError')
      map.delete(k)
    },
  }
}

const parseBot = (raw: string) => (raw.startsWith('bot:') ? raw.slice(4) : null)

test('重整回到自己的位置：分頁自己的（sessionStorage）優先於所有分頁共用的（localStorage）', () => {
  const session = fakeStore({ sel: 'bot:X' })
  const local = fakeStore({ sel: 'bot:Y' }) // 另一個分頁最後選的
  assert.equal(readTabValue('sel', parseBot, [session, local]), 'X')
})

test('新開的分頁（sessionStorage 沒有）才退回共用的最後一次', () => {
  const session = fakeStore()
  const local = fakeStore({ sel: 'bot:Y' })
  assert.equal(readTabValue('sel', parseBot, [session, local]), 'Y')
})

test('分頁自己的值讀不到（丟例外）或解析不了（壞掉）就換下一個來源', () => {
  assert.equal(readTabValue('sel', parseBot, [fakeStore({ sel: 'bot:X' }, true), fakeStore({ sel: 'bot:Y' })]), 'Y')
  assert.equal(readTabValue('sel', parseBot, [fakeStore({ sel: '{壞掉' }), fakeStore({ sel: 'bot:Y' })]), 'Y')
  assert.equal(readTabValue('sel', parseBot, [undefined, null]), null)
})

test('寫入兩邊都寫：分頁自己的（重整用）與共用的（新分頁用）；寫不進去的那邊不影響另一邊', () => {
  const session = fakeStore()
  const local = fakeStore()
  writeTabValue('sel', 'bot:X', [session, local])
  assert.equal(session.map.get('sel'), 'bot:X')
  assert.equal(local.map.get('sel'), 'bot:X')

  const broken = fakeStore({}, true)
  const ok = fakeStore()
  assert.doesNotThrow(() => writeTabValue('sel', 'bot:Z', [broken, ok]))
  assert.equal(ok.map.get('sel'), 'bot:Z')

  writeTabValue('sel', null, [session, local])
  assert.equal(session.map.has('sel'), false)
  assert.equal(local.map.has('sel'), false)
})
