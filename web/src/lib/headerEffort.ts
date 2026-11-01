import { effortLabel } from '../api/types'

/** 標題列模型旁的強度縮寫（2026-11-01 使用者：模型選單維持全名，只有標題列縮）：Medium=M、Max=X；只換顯示，原值不變。 */
export function headerEffortLabel(effort: string): string {
  const short: Record<string, string> = { low: 'L', medium: 'M', high: 'H', xhigh: 'XH', max: 'X', ultra: 'U' }
  return short[effort.toLowerCase()] ?? effortLabel(effort)
}
