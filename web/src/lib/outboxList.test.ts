import { test } from 'node:test'
import assert from 'node:assert/strict'
import { emptyReason, fileSize, lastSettledTurnKey, orderFiles, remainingLabel, remainingNow } from './outboxList'

const file = (name: string, modified: number, remainingSecs = 3600) => ({ name, size: 1, modified, remainingSecs })

test('檔案大小講的是「下載會多大」，個位數才給小數', () => {
  assert.equal(fileSize(0), '0 B')
  assert.equal(fileSize(-1), '0 B', '壞數字不要畫成 NaN')
  assert.equal(fileSize(512), '512 B')
  assert.equal(fileSize(18_432), '18 KB')
  assert.equal(fileSize(1024 * 1.5), '1.5 KB')
  assert.equal(fileSize(1024 ** 2 * 1.5), '1.5 MB')
  assert.equal(fileSize(1024 ** 2 * 64), '64 MB')
})

test('剩餘時間從讀清單那一刻往下扣，不看瀏覽器時鐘跟 daemon 差多少', () => {
  const fetchedAt = 1_789_600_000_000
  const f = file('a.md', 1, 3000)
  assert.equal(remainingNow(f, fetchedAt, fetchedAt), 3000)
  assert.equal(remainingNow(f, fetchedAt, fetchedAt + 600_000), 2400, '十分鐘後')
  assert.equal(remainingNow(f, fetchedAt, fetchedAt + 4_000_000), 0, '過期不出現負數')
  assert.equal(remainingNow(f, fetchedAt, fetchedAt - 60_000), 3000, '時鐘往回跳不加時間')
})

test('倒數以分鐘講、無條件進位，到期講即將清除', () => {
  assert.equal(remainingLabel(3600), '剩 60 分鐘')
  assert.equal(remainingLabel(2521), '剩 43 分鐘')
  assert.equal(remainingLabel(30), '剩 1 分鐘', '還沒到期就不能講 0 分鐘')
  assert.equal(remainingLabel(0), '即將清除')
  assert.equal(remainingLabel(Number.NaN), '即將清除')
})

test('沒有檔案的每一種原因都要講清楚，不能只留一片空白', () => {
  assert.match(emptyReason(null, false), /先選一顆 bot/)
  assert.match(emptyReason('outbox_remote', true), /遠端主機/)
  assert.match(emptyReason(null, true), /AM_OUTBOX/)
  assert.match(emptyReason(null, true), /1 小時/)
  assert.ok(emptyReason('something_new', true).length > 0, '認不得的原因也要有話講')
})

test('新的排前面，同一秒的依名字排（順序不要每次重整都在跳）', () => {
  const out = orderFiles([file('b.txt', 100), file('a.txt', 100), file('newest.txt', 200), file('old.txt', 50)])
  assert.deepEqual(out.map((x) => x.name), ['newest.txt', 'a.txt', 'b.txt', 'old.txt'])
})

test('outbox 被判不可信時講清楚，不說成還沒有檔案', () => {
  const text = emptyReason('outbox_untrusted', true)
  assert.match(text, /符號連結|擁有者/)
  assert.doesNotMatch(text, /還沒有檔案/)
})

test('回合一結束清單就該重讀：鍵跟著最近一個結束的回合變，還在跑的不算', () => {
  const t = (id: string, status: string, completed_at: string | null) => ({ id, status, completed_at }) as never
  assert.equal(lastSettledTurnKey(undefined), '')
  assert.equal(lastSettledTurnKey({ a: t('a', 'in_flight', null) }), '', '還在跑：不重讀')
  const before = lastSettledTurnKey({ a: t('a', 'completed', '2026-09-16T12:28:00Z') })
  assert.equal(before, 'a:2026-09-16T12:28:00Z')
  // 下一則送出、還在跑：鍵不變。
  assert.equal(lastSettledTurnKey({ a: t('a', 'completed', '2026-09-16T12:28:00Z'), b: t('b', 'in_flight', null) }), before)
  // 跑完了：鍵變了 → 重讀。
  assert.equal(
    lastSettledTurnKey({ a: t('a', 'completed', '2026-09-16T12:28:00Z'), b: t('b', 'completed', '2026-09-16T12:36:30Z') }),
    'b:2026-09-16T12:36:30Z',
  )
  // 失敗結束的回合也算（bot 可能放了一半）。
  assert.equal(lastSettledTurnKey({ c: t('c', 'failed', '2026-09-16T12:40:00Z') }), 'c:2026-09-16T12:40:00Z')
})
