/**
 * issue #23：`refreshState` 的 single-flight + trailing 合併。連發 N 個 frame 最多打兩次
 * `GET /api/state`，且回應有序，舊快照不會蓋掉新的。
 */
export function singleFlight(run: () => Promise<void>, onError?: (e: unknown) => void): () => Promise<void> {
  let inflight: Promise<void> | null = null
  let again = false
  // 迴圈還會不會再看 `again`。收尾是同步的（`finally` 裡只設這個旗標），`inflight` 要等 `.finally` 才清，
  // 中間進來的呼叫不能當成合併：迴圈已經不會再跑，那次刷新會被吞掉（#1690）。
  let looping = false
  return () => {
    if (inflight && looping) {
      again = true
      return inflight
    }
    looping = true
    const p: Promise<void> = (async () => {
      try {
        do {
          again = false
          try {
            await run()
          } catch (e) {
            if (!onError) throw e
            onError(e)
          }
        } while (again)
      } finally {
        looping = false
      }
    })().finally(() => {
      if (inflight === p) inflight = null
    })
    inflight = p
    return p
  }
}

/** 保險：繞過 single-flight 的舊 `daemon_seq` 快照丟掉。回傳新的已套用 seq；null = 丟。 */
export function acceptStateSeq(applied: number, incoming: number): number | null {
  if (!Number.isFinite(incoming)) return applied
  return incoming < applied ? null : incoming
}
