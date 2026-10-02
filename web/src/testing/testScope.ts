/**
 * 「這段程式是哪一條測試的 body 在跑、那條測試結束了沒」（issue #770）。
 *
 * bun 判一條測試逾時之後只是不等它、往下跑，**它的 async body 不會被取消**：還會繼續 `await`、繼續進 `act()`。
 * 那個 `act()` 跟下一條測試的 `act()` 重疊時，React 的 act 深度會錯位（「overlapping act() calls」），之後同一個
 * 行程裡的 act 都不再 flush——一條測試在負載下逾時，後面每個 DOM 測試檔的 render 都看不到（實測 1 條逾時拖紅 10 條）。
 *
 * `test/node-test-shim.ts` 把每條測試的 body 包進 `runTestBody`；`domHarness` 的 `act` 用 `endedTestScope()` 認出
 * 「已經結束的測試還在跑的 body」並拒絕它，`unmountAll` 用 `endCurrentTest()` 在收尾當下就把那條標成已結束。
 */
import { AsyncLocalStorage } from 'node:async_hooks'

export interface TestScope {
  readonly name: string
  ended: boolean
}

const scopes = new AsyncLocalStorage<TestScope>()
let current: TestScope | null = null

/** 在新的 scope 裡跑一條測試的 body；body 裡排的 timer／promise 都帶著這個 scope。 */
export function runTestBody<T>(name: string, body: () => T): T {
  const scope: TestScope = { name, ended: false }
  current = scope
  return scopes.run(scope, body)
}

/** 目前（最後開始的）那條測試已經結束：通過、失敗或逾時都一樣，afterEach 呼叫。 */
export function endCurrentTest(): void {
  if (current) current.ended = true
}

/** 呼叫端屬於一條已經結束的測試（逾時後還在跑的 body）就回那條的 scope；hook 或還在跑的測試回 `null`。 */
export function endedTestScope(): TestScope | null {
  const scope = scopes.getStore()
  return scope?.ended ? scope : null
}

/** 呼叫端所在的 scope（hook 裡是 `undefined`）。 */
export function currentTestScope(): TestScope | undefined {
  return scopes.getStore()
}
