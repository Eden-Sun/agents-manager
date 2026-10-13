import type { Run } from '../api/types'

/**
 * codex「模型滿載」（OpenAI 那頭的容量不夠，#1673／#1674）：daemon 在 `run.turn_error_kind` 帶結構化的種類，
 * 網頁只認這個值、不比對 `turn_error` 的文字。下一個回合開始 daemon 會清掉 `turn_error`，種類跟著變 null。
 */
export const MODEL_AT_CAPACITY = 'model_at_capacity'

export function modelAtCapacity(run: Pick<Run, 'turn_error_kind'> | null | undefined): boolean {
  return run?.turn_error_kind === MODEL_AT_CAPACITY
}

/** 側欄／晶片／標題列共用的短字。 */
export const CAPACITY_LABEL = '模型滿載'
/** tooltip：講清楚它不是卡住、也不是額度，也不會自己重試。 */
export const CAPACITY_TITLE = '模型滿載：OpenAI 目前容量不夠，這一回合沒有跑；不是 bot 卡住、也不是額度用完。換個模型或稍後再送一次（不會自動重送、也不會自動換模型）'
