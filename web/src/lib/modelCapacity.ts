import type { Run } from '../api/types'
import { msg } from './i18n'

export const MODEL_AT_CAPACITY = 'model_at_capacity'

export function modelAtCapacity(run: Pick<Run, 'turn_error_kind'> | null | undefined): boolean {
  return run?.turn_error_kind === MODEL_AT_CAPACITY
}

export const CAPACITY_LABEL = msg('模型滿載')
export const CAPACITY_TITLE = msg('模型滿載：OpenAI 目前容量不夠，這一回合沒有跑；不是 bot 卡住、也不是額度用完。換個模型或稍後再送一次（不會自動重送、也不會自動換模型）')
