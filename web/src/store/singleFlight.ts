/**
 * issue #23：`refreshState` 的 single-flight + trailing 合併。
 *
 * 同一時間最多只有一個 `run` 在跑；跑的時候再被叫到，只記一個「跑完再跑一次」的旗標，
 * 所有等待者共用同一個 promise。這樣 daemon 一次操作連發 N 個 frame，前端最多打
 * 兩次 `GET /api/state`（一次進行中、一次收尾），而且回應天生有序，舊快照不會蓋掉新的。
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

/**
 * 保險：即使有東西繞過 single-flight 並行打 state，`daemon_seq` 比上次套用過的還舊的快照直接丟掉。
 * 回傳新的「已套用 seq」；回傳 null 表示這份快照要丟。
 */
export function acceptStateSeq(applied: number, incoming: number): number | null {
  if (!Number.isFinite(incoming)) return applied
  return incoming < applied ? null : incoming
}
