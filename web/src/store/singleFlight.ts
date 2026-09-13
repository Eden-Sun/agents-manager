/**
 * issue #23：`refreshState` 的 single-flight + trailing 合併。連發 N 個 frame 最多打兩次
 * `GET /api/state`，且回應有序，舊快照不會蓋掉新的。
 */
export function singleFlight(run: () => Promise<void>, onError?: (e: unknown) => void): () => Promise<void> {
  let inflight: Promise<void> | null = null
  let again = false
  return () => {
    if (inflight) {
      again = true
      return inflight
    }
    inflight = (async () => {
      do {
        again = false
        try {
          await run()
        } catch (e) {
          if (!onError) throw e
          onError(e)
        }
      } while (again)
    })().finally(() => {
      inflight = null
    })
    return inflight
  }
}

/** 保險：繞過 single-flight 的舊 `daemon_seq` 快照丟掉。回傳新的已套用 seq；null = 丟。 */
export function acceptStateSeq(applied: number, incoming: number): number | null {
  if (!Number.isFinite(incoming)) return applied
  return incoming < applied ? null : incoming
}
