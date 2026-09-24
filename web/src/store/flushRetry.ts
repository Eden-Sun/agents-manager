/**
 * 排隊訊息重送的節奏（issue #530）。
 *
 * `flushQueued` 以前是「每收到一幀 `bot_status`／`turn_updated` 就再送一次」：撞上 daemon 的 retryable 409
 * （`composer_busy`——有人正在那顆 bot 的終端打字最常見）時，幀來幾次就送幾次、跳幾則一模一樣的 toast，
 * 而且沒有停下來的一天。這裡只放「隔多久再送、送幾次放棄」的純規則，狀態機在 `store.ts`。
 *
 * 退避從 1 秒起、每次加倍、封頂 30 秒；連同第一次共 7 次之後停手，訊息留在佇列裡等使用者處置——
 * 一直重試跟一直不重試都不對，至少要停在一個講得出來的地方。
 */

/** 第一次不是重試，是原本就有的防抖（等 daemon 把回合收乾淨）。 */
export const FLUSH_FIRST_MS = 350

/** 第 n 次重試等多久（1s、2s、4s、8s、16s、30s）。 */
export const FLUSH_BACKOFF_MS = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000] as const

let backoffMs: readonly number[] = FLUSH_BACKOFF_MS

/**
 * 測試用：整套退避跑到上限要 61 秒，測試不可能等。換成很短的一份（`null` 還原）——
 * 同 `setOrderSaveSignalForTest`，靠真的計時來測只會在負載高時假紅。
 */
export function setFlushBackoffForTest(ms: readonly number[] | null) {
  backoffMs = ms && ms.length > 0 ? ms : FLUSH_BACKOFF_MS
}

/** 第一次 + 每一段退避各一次。 */
export function flushMaxAttempts(): number {
  return backoffMs.length + 1
}

/** 已經失敗 `failures` 次之後，下一次要等多久。 */
export function flushDelayMs(failures: number): number {
  if (failures <= 0) return FLUSH_FIRST_MS
  return backoffMs[Math.min(failures, backoffMs.length) - 1]
}

/** 還能不能再送。 */
export function flushGaveUp(failures: number): boolean {
  return failures >= flushMaxAttempts()
}

/** 放棄時講一次的話：要講清楚訊息還在、以及人可以做什麼。 */
export function flushGaveUpText(failures: number): string {
  return `排隊的訊息送了 ${failures} 次都沒送出去，先停下來了。訊息還留在佇列裡——清掉那顆 bot 終端輸入框裡的字，再用「退回輸入框」重送一次。`
}
