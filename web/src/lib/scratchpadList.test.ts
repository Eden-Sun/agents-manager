import { test } from 'node:test'
import assert from 'node:assert/strict'
import { emptyReason, fileSize, modifiedAgo, orderFiles } from './scratchpadList'

test('檔案大小講的是「下載會多大」，個位數才給小數', () => {
  assert.equal(fileSize(0), '0 B')
  assert.equal(fileSize(-1), '0 B', '壞數字不要畫成 NaN')
  assert.equal(fileSize(512), '512 B')
  assert.equal(fileSize(18_432), '18 KB')
  assert.equal(fileSize(1024 * 1.5), '1.5 KB')
  assert.equal(fileSize(1024 ** 2 * 1.5), '1.5 MB')
  assert.equal(fileSize(1024 ** 2 * 64), '64 MB')
})

test('修改時間是相對的，時鐘往回跳不會出現負數', () => {
  const now = 1_789_600_000_000
  const at = (s: number) => Math.floor(now / 1000) - s
  assert.equal(modifiedAgo(at(10), now), '剛剛')
  assert.equal(modifiedAgo(at(12 * 60), now), '12 分鐘前')
  assert.equal(modifiedAgo(at(3 * 3600), now), '3 小時前')
  assert.equal(modifiedAgo(at(3 * 86400), now), '3 天前')
  assert.equal(modifiedAgo(Math.floor(now / 1000) + 600, now), '剛剛', '未來的時間當剛剛')
  assert.equal(modifiedAgo(0, now), '')
})

test('沒有檔案的每一種原因都要講清楚，不能只留一片空白', () => {
  assert.match(emptyReason(null, false), /先選一顆 bot/)
  assert.match(emptyReason('scratchpad_remote', true), /遠端主機/)
  assert.match(emptyReason('scratchpad_no_session', true), /還沒跑過/)
  assert.match(emptyReason('scratchpad_missing', true), /還沒建立/)
  assert.match(emptyReason(null, true), /還沒有檔案/)
  // 認不得的原因也要有話講，不能回 undefined。
  assert.ok(emptyReason('something_new', true).length > 0)
})

test('新的排前面，同一秒的依名字排（順序不要每次重整都在跳）', () => {
  const f = (name: string, modified: number) => ({ name, size: 1, modified })
  const out = orderFiles([f('b.txt', 100), f('a.txt', 100), f('newest.txt', 200), f('old.txt', 50)])
  assert.deepEqual(out.map((x) => x.name), ['newest.txt', 'a.txt', 'b.txt', 'old.txt'])
})
