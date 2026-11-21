import test from 'node:test'
import assert from 'node:assert/strict'

test('兩個分頁各自新增的檔案用不同的鍵', async () => {
  const tabA = await import('./shelf.ts')
  const otherTab = './shelf.ts?tab=b'
  const tabB = (await import(otherTab)) as typeof tabA
  tabA.useShelf.getState().add([new File(['a'], 'a.pdf', { type: 'application/pdf' })])
  tabB.useShelf.getState().add([new File(['b'], 'b.pdf', { type: 'application/pdf' })])
  const keysA = tabA.useShelf.getState().items.map((it) => it.key)
  const keysB = tabB.useShelf.getState().items.map((it) => it.key)
  assert.equal(keysA.length, 1)
  assert.equal(keysB.length, 1)
  assert.notEqual(keysA[0], keysB[0])
})

type Shelf = typeof import('./shelf.ts')

function tabRandom(tab: string): number {
  return (parseInt(tab, 36) + 0.25) / 36 ** 6
}

async function loadTab(query: string, tab: string): Promise<Shelf> {
  const real = Math.random
  Math.random = () => tabRandom(tab)
  try {
    return (await import(`./shelf.ts?${query}`)) as Shelf
  } finally {
    Math.random = real
  }
}

const pdf = (name: string) => new File([name], `${name}.pdf`, { type: 'application/pdf' })

test('前綴像科學記號的舊鍵還原後，新增的檔案鍵不重複（#1624）', async () => {
  const prior = await loadTab('prior', '1234e5')
  prior.useShelf.getState().add([pdf('x')])
  const saved = prior.useShelf.getState().items.map((it) => ({ key: it.key, file: it.file, addedAt: it.addedAt }))
  assert.deepEqual(
    saved.map((s) => s.key),
    ['s1234e51'],
  )

  const next = await loadTab('next', 'abcdef')
  next.useShelf.getState().restore(saved)
  next.useShelf.getState().add([pdf('a'), pdf('b')])
  const keys = next.useShelf.getState().items.map((it) => it.key)
  assert.equal(keys.length, 3)
  assert.equal(new Set(keys).size, 3, `鍵重複：${keys.join(', ')}`)
})

test('舊版 s<n> 鍵還原後，新增的檔案接在計數後面', async () => {
  const next = await loadTab('legacy', 'abcdef')
  next.useShelf.getState().restore([{ key: 's7', file: pdf('old'), addedAt: 0 }])
  next.useShelf.getState().add([pdf('new')])
  const keys = next.useShelf.getState().items.map((it) => it.key).sort()
  assert.deepEqual(keys, ['s7', 'sabcdef8'])
})
