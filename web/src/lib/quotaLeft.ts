/**
 * 額度視窗「剩餘 %」的顯示值。頂端 QuotaStrip 與側欄 bot 列的警示小標共用這一份，
 * 免得同一個視窗兩邊讀數對不上（頂端 `1.6`、側欄 `2%`，2026-11-04 使用者）。
 * 不到 10 留一位小數：快用完時 9.8 與 9.1 差一整回合（2026-09-08）。
 */
export function leftPct(usedPct: number): number {
  const left = Math.max(0, 100 - usedPct)
  return left < 10 ? Math.round(left * 10) / 10 : Math.round(left)
}
