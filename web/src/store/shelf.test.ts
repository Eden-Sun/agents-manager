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
