/**
 * issue #457：按「載入更早的」補回來的那一段，不能在下一則新訊息進來時被 `MESSAGE_CAP` 從頭切掉。
 *
 * 這裡只測 `messageCap.ts` 的純函式。真的按下去那一段（`loadEarlierMessages` → 下一則新訊息
 * 剪不剪得到）在 `storeActions.test.ts`：驅動 `useStore` 的 harness 假設「一個行程只有一個擁有者」，
 * 兩個檔案同時操作那顆 singleton 會互相踩（實測會讓另一個檔案紅 51 條）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MESSAGE_CAP } from './lists.ts'
import { capFor, clearFloor, raiseFloor } from './messageCap.ts'

test('沒抬過就是 MESSAGE_CAP；抬過就用抬過的那個', () => {
  assert.equal(capFor({}, 'b1'), MESSAGE_CAP)
  assert.equal(capFor({ b1: 1200 }, 'b1'), 1200)
  assert.equal(capFor({ b1: 1200 }, 'b2'), MESSAGE_CAP, '一個對話的下緣不影響別的')
  assert.equal(capFor({ b1: 10 }, 'b1'), MESSAGE_CAP, '比 MESSAGE_CAP 小的值不該把上限拉低')
})

test('補回歷史就抬下緣：留住現有的，另外給 MESSAGE_CAP 則新訊息的餘裕', () => {
  // #457 的重現情境：清單已經在 500，補回 200 → 700。
  const floors = raiseFloor({}, 'b1', 200, 700)
  assert.equal(floors.b1, 700 + MESSAGE_CAP)
  // 關鍵不變量：補完之後的長度嚴格小於上限，所以下一則新訊息剪不到任何東西。
  assert.ok(capFor(floors, 'b1') > 700)
})

test('那一頁沒有補到新東西就不抬（全是重複、或本來就沒有更早的）', () => {
  assert.deepEqual(raiseFloor({}, 'b1', 0, 500), {}, 'added=0 不抬')
  assert.deepEqual(raiseFloor({}, 'b1', -1, 500), {}, '負數當沒補到')
  const once = raiseFloor({}, 'b1', 200, 700)
  assert.equal(raiseFloor(once, 'b1', 0, 700).b1, once.b1, '再按一次沒補到東西，下緣不動')
})

test('連按幾次只升不降，不會把上一次的餘裕吃掉', () => {
  let floors = raiseFloor({}, 'b1', 200, 700)
  const first = floors.b1
  floors = raiseFloor(floors, 'b1', 200, 900)
  assert.ok(floors.b1 > first, `第二次要更高：${first} → ${floors.b1}`)
  assert.equal(raiseFloor(floors, 'b1', 200, 600).b1, floors.b1, '算出比較小的長度也不退回去')
})

test('整頁重灌就歸零：那一頁自己是新的基準', () => {
  const floors = raiseFloor({}, 'b1', 200, 700)
  assert.deepEqual(clearFloor(floors, 'b1'), {})
  assert.equal(clearFloor({}, 'b1').b1, undefined, '沒有那一鍵也不炸')
  const two = raiseFloor(raiseFloor({}, 'b1', 200, 700), 'b2', 200, 700)
  assert.equal(clearFloor(two, 'b1').b2, two.b2, '只清自己那一鍵')
})
