import test from 'node:test'
import assert from 'node:assert/strict'
import { useShelf } from './shelf.ts'

test.beforeEach(() => useShelf.getState().clear())
test.afterEach(() => useShelf.getState().clear())

test('adding before IndexedDB restore preserves a legacy s1 item', () => {
  const freshFile = new File(['new'], 'new.txt', { type: 'text/plain' })
  const oldFile = new File(['old'], 'old.txt', { type: 'text/plain' })

  assert.equal(useShelf.getState().add([freshFile]).added, 1)
  const added = useShelf.getState().items.find((item) => item.file === freshFile)
  assert.ok(added)

  useShelf.getState().restore([{ key: 's1', file: oldFile, addedAt: added.addedAt - 1 }])

  const items = useShelf.getState().items
  assert.equal(items.length, 2, 'the pre-restore item and persisted item must both remain')
  assert.ok(items.some((item) => item.key === 's1' && item.file === oldFile))
  assert.ok(items.some((item) => item.file === freshFile))
  assert.doesNotMatch(added.key, /^s\d+$/, 'new keys must not reuse the legacy numeric key namespace')
})

test('shelf keys remain available when randomUUID is unavailable in an HTTP context', () => {
  const originalRandomUUID = crypto.randomUUID
  Object.defineProperty(crypto, 'randomUUID', { value: undefined, configurable: true })
  try {
    const file = new File(['lan'], 'lan.txt', { type: 'text/plain' })
    assert.equal(useShelf.getState().add([file]).added, 1)
    const added = useShelf.getState().items.find((item) => item.file === file)
    assert.ok(added)
    assert.doesNotMatch(added.key, /^s\d+$/)
  } finally {
    Object.defineProperty(crypto, 'randomUUID', { value: originalRandomUUID, configurable: true })
  }
})
