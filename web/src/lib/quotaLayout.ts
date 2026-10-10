/**
 * 桌機標題列額度區的三個讓位階段（收縮優先序：附屬 chip／遠端記憶體先讓，額度量表最後才收）：
 * 0＝完整量表＋遠端主機名牌；1＝完整量表、收掉名牌（標題列左邊已有 `@host` 徽章）；2＝收合：每格直向疊出全部窗口（放不下的尾巴裁掉，#1158）。
 * 本機沒有名牌，1 跟 0 長得一樣，直接跳過。
 */
export type QuotaFit = 0 | 1 | 2

export interface QuotaFitState {
  level: QuotaFit
  /** `need[n]`＝在階段 n 量到的內容寬；寬度回到這個數才退回階段 n。沒量過是 null。 */
  need: [number | null, number | null]
}

export const QUOTA_FIT_START: QuotaFitState = { level: 0, need: [null, null] }

/** 次像素誤差：rect 寬是小數，差不到半格像素不算放不下。 */
const SLACK = 0.5

/**
 * 依量到的寬度決定下一個階段。`avail`＝額度區能拿到的寬（遠端記憶體會自己縮，算在可用裡），
 * `content`＝目前階段每一格照內容排開的寬（格子不縮到比內容小）。
 *
 * 不直接拿固定門檻（舊的 340＋格數×116）：標題列上還有遠端記憶體、主機名牌、更新 chip，寬度隨主機與資料變，
 * 固定數字在遠端主機時差了約 170px，五格互相疊字（2026-09-28 使用者截圖）。
 * 收合後內容變窄，要回到比「上次放不下時量到的內容寬」還寬才退回——這就是遲滯，不會在兩個階段之間來回跳。
 */
export function nextQuotaFit(s: QuotaFitState, avail: number, content: number, hasHostTag: boolean): QuotaFitState {
  if (s.level !== 2 && content > avail + SLACK) {
    const need: QuotaFitState['need'] = [...s.need]
    need[s.level] = content
    // 本機沒有名牌可收：階段 1 的內容跟 0 一樣寬，一定也放不下。
    if (s.level === 0 && !hasHostTag) need[1] = content
    const level: QuotaFit = s.level === 0 && hasHostTag ? 1 : 2
    return { level, need }
  }
  if (s.level > 0) {
    const down = (s.level - 1) as 0 | 1
    const target = down === 1 && !hasHostTag ? 0 : down
    const need = s.need[target]
    if (need === null || avail + SLACK >= need) return { level: target, need: s.need }
  }
  return s
}
