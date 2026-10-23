import { settle } from './domHarness'

/**
 * 把「fetch→setState」這類純 promise／macrotask 的後續工作跑完：連續讓出幾輪 macrotask（每輪都在 `act` 裡）。
 * 給後端是 mock／假 fetch、沒有牆鐘延遲的測試，取代 `settle(100)`、`settle(300)` 這種睡真時間的硬等；
 * 等的事情若真的要時間（計時器），改用 `until(條件)` 或手動時鐘，不要用它。
 */
export async function flush(rounds = 8): Promise<void> {
  for (let i = 0; i < rounds; i++) await settle()
}
