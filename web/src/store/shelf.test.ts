import test from 'node:test'
import assert from 'node:assert/strict'

// IndexedDB 是同源的分頁共用的：兩個分頁各自載入 shelf（兩份模組狀態），各自新增的圖片不能用同一把鍵，
// 否則後寫的會覆蓋先寫的那一列，重整後拿到的是別人的檔案。
test('兩個分頁各自新增的檔案用不同的鍵', async () => {
  const tabA = await import('./shelf.ts')
  // 另一個模組實例：查詢字串讓它是獨立的一份（變數路徑，免得型別檢查去解析它）。
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

/** 模組載入時 `Math.random()` 只呼叫一次，讓它吐出的 base36 前六位正好是 `tab`。 */
function tabRandom(tab: string): number {
  return (parseInt(tab, 36) + 0.25) / 36 ** 6
}

/** 用固定的隨機前綴載入一份獨立的 shelf 模組實例，模擬一個分頁。 */
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

// #1624：新鍵是 `s<隨機前綴><seq>`，前綴可能剛好長成 `1234e5` 這種科學記號。還原時不能把它當計數器解析，
// 否則 seq 變成 1.234e54，之後 seq += 1 不再改變數值，同一批新增的檔案會拿到同一把鍵互相覆蓋。
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

// 舊版 `s<n>` 的計數鍵仍要能還原，還原後新增的檔案接在它後面。
test('舊版 s<n> 鍵還原後，新增的檔案接在計數後面', async () => {
  const next = await loadTab('legacy', 'abcdef')
  next.useShelf.getState().restore([{ key: 's7', file: pdf('old'), addedAt: 0 }])
  next.useShelf.getState().add([pdf('new')])
  const keys = next.useShelf.getState().items.map((it) => it.key).sort()
  assert.deepEqual(keys, ['s7', 'sabcdef8'])
})
