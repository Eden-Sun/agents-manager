import { test } from 'node:test'
import assert from 'node:assert/strict'
import { downloadFailure, emptyReason, fileSize, isPreviewableImage, lastSettledTurnKey, orderFiles, previewPlacement, readOutbox, remainingLabel, remainingNow } from './outboxList'
import { ApiError } from '../api/types'

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

/**
 * #234：`GET /api/bots/{id}/outbox` 失敗（網路、500、404）以前被當成「沒有檔案」：`reason: null` 的空清單，畫面寫「還沒有檔案。bot 把要給你的
 * 檔案放進 $AM_OUTBOX…」——bot 明明放了檔案，使用者卻被告知沒有，還照著說明重放一次。讀不到與真的沒檔分不開。
 */
test('讀不到清單不是「還沒有檔案」：講讀不到、指路去按重新讀取', () => {
  const text = emptyReason('load_failed', true)
  assert.doesNotMatch(text, /還沒有檔案/)
  assert.match(text, /讀不到/)
  assert.match(text, /重新讀取|↻/)
})

test('讀清單失敗回 load_failed，不是 reason 為 null 的空清單；成功的照舊排序、原樣帶 reason', async () => {
  for (const err of [new TypeError('Failed to fetch'), new Error('HTTP 500'), Object.assign(new Error('not found'), { status: 404 })]) {
    const out = await readOutbox(() => Promise.reject(err))
    assert.deepEqual(out, { dir: '', reason: 'load_failed', ttlSecs: 0, files: [] }, String(err))
  }
  const ok = await readOutbox(() => Promise.resolve({ dir: '/o', reason: null, ttlSecs: 3600, files: [file('a.md', 1), file('b.md', 9)] }))
  assert.deepEqual(ok.files.map((f) => f.name), ['b.md', 'a.md'])
  assert.equal(ok.reason, null)
  // 遠端的 200 帶 reason：原樣帶著，不被當成失敗。
  assert.equal((await readOutbox(() => Promise.resolve({ dir: '', reason: 'outbox_remote', ttlSecs: 3600, files: [] }))).reason, 'outbox_remote')
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

test('只有瀏覽器畫得出來、daemon 也肯給 image/* 的圖檔才預覽', () => {
  for (const n of ['a.png', 'B.PNG', 'c.jpg', 'd.jpeg', 'e.gif', 'f.webp']) assert.equal(isPreviewableImage(n), true, n)
  for (const n of ['a.svg', 'png', 'a.png.txt', 'report.md', 'x.log']) assert.equal(isPreviewableImage(n), false, n)
})

test('預覽框放在那一列左邊、夾在視窗內；左邊放不下改放下方', () => {
  const vp = { width: 1600, height: 1000 }
  const box = { width: 360, height: 300 }
  assert.deepEqual(previewPlacement({ left: 1250, top: 600, bottom: 660 }, box, vp), { left: 882, top: 600 })
  // 靠近底部：往上推，不能超出視窗
  assert.deepEqual(previewPlacement({ left: 1250, top: 900, bottom: 960 }, box, vp), { left: 882, top: 692 })
  // 窄視窗（手機）：左邊不夠 → 放下方，左緣夾在視窗內
  assert.deepEqual(previewPlacement({ left: 100, top: 100, bottom: 150 }, box, { width: 390, height: 800 }), { left: 22, top: 158 })
})

test('下載失敗講人話：404 是「被清掉了」並要求呼叫端移除那一列（issue #547）', () => {
  const gone = downloadFailure('shot.png', new ApiError(404, { error: 'not_found', what: 'file' }, 'GET … failed (404)'))
  assert.equal(gone.gone, true)
  assert.match(gone.text, /shot\.png.*不在了/)
  assert.doesNotMatch(gone.text, /Not Found|404/)

  const big = downloadFailure('dump.bin', new ApiError(409, { error: 'conflict', reason: 'file_too_large', size: 60 * 1024 ** 2, max: 50 * 1024 ** 2 }, 'x'))
  assert.equal(big.gone, false)
  assert.match(big.text, /60 MB.*50 MB/)

  assert.match(downloadFailure('a.txt', new ApiError(409, { error: 'conflict', reason: 'outbox_remote', host: 'mini' }, 'x')).text, /遠端主機/)
  // 認不出來的 reason 至少帶狀態碼；不是 ApiError 的照原樣。
  assert.match(downloadFailure('a.txt', new ApiError(500, { error: 'upstream', message: '壞了' }, 'x')).text, /HTTP 500/)
  assert.match(downloadFailure('a.txt', new Error('連線中斷')).text, /連線中斷/)
})
