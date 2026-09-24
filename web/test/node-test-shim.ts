// bun test 的 preload（web/bunfig.toml）：把 `node:test` 直接對到 `bun:test` 的 test（#426）。
//
// bun 1.3.14 內建的 node:test 墊片用一個模組層級的 `ctx` 記「現在在哪個 test 裡」，測試開始時設、
// 結束時還原；同一檔裡某條 async 測試失敗、後面還有測試時，還原的順序會錯，`ctx` 就一直指著已經結束的那條。
// 之後每個檔在載入時呼叫 `test()` 都被當成「test() inside another test()」而整檔報錯——一條測試紅，
// 後面 20 個檔、136 條測試跟著不見（2026-09-24 實測，最小重現在 #426）。
//
// web 的測試只用 `test(name, fn)` 與 `test.beforeEach`（沒有 context 參數、沒有 options），bun:test 直接相容；
// 這裡把 node:test 常見的成員都對上。用到別的 API（`t.mock`、`t.signal`…）要在這裡補，不然會是 undefined。
// 上游 1.4 已經重寫了 node:test，升級 bun 後可以拿掉這個 preload。
import { afterAll, afterEach, beforeAll, beforeEach, describe, it, mock, test as bunTest } from 'bun:test'

const test = Object.assign((...args: Parameters<typeof bunTest>) => bunTest(...args), {
  describe,
  it,
  before: beforeAll,
  after: afterAll,
  beforeEach,
  afterEach,
})

// skip／todo／only 用 getter，用到才讀：CI（`CI=true`）下 bun 光是讀 `test.only` 就丟
// 「.only is disabled in CI environments」，載入時就讀的話每個測試檔都紅（#429）。
for (const key of ['skip', 'todo', 'only'] as const) {
  Object.defineProperty(test, key, { get: () => bunTest[key], enumerable: true })
}

mock.module('node:test', () => ({
  default: test,
  test,
  it,
  describe,
  before: beforeAll,
  after: afterAll,
  beforeEach,
  afterEach,
}))
