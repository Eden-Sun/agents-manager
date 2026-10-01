/**
 * 更新框「AGM 解析」的重讀時機。daemon 的交辦每次轉換（AGM 接手、給出結論）都推 `supervisor_changed`，store 把它數成
 * `supervisorRev`；框本身沒有任何輪詢。只有**還沒有結論**才值得讀（沒派、派了還沒回）：已經 `done` 就不再打 API。
 * 一陣事件（每次 ack 都推一則）合成一次讀：`delayMs` 之後才跑，下一個 rev 來了前一個要取消。回傳取消函式。
 */
export function scheduleReviewRefresh(opts: {
  rev: number
  state: 'none' | 'pending' | 'done' | undefined
  run: () => void
  delayMs?: number
}): () => void {
  if (opts.rev === 0 || opts.state === 'done') return () => {}
  const t = setTimeout(opts.run, opts.delayMs ?? 1000)
  return () => clearTimeout(t)
}
