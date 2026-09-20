/**
 * 側欄搜尋的內容命中：debounce 之後問 daemon，晚到的舊結果不能寫回去。
 * 清空搜尋框也算一次「新的查詢」——不讓 seq 前進的話，清空前送出的請求晚到仍會通過守衛，
 * 在沒有搜尋的狀態下留下命中筆數與 snippet。
 */
export function createHitSearch<T>(search: (q: string) => Promise<T>, apply: (hits: T | null) => void, delayMs = 250) {
  let seq = 0
  let timer: ReturnType<typeof setTimeout> | null = null
  const stop = () => {
    if (timer) clearTimeout(timer)
    timer = null
  }
  return {
    run(query: string) {
      stop()
      const q = query.trim()
      const mine = ++seq
      if (!q) {
        apply(null)
        return
      }
      timer = setTimeout(() => {
        void search(q)
          .then((r) => {
            if (seq === mine) apply(r)
          })
          // 失敗時屬性搜尋照常。
          .catch(() => {
            if (seq === mine) apply(null)
          })
      }, delayMs)
    },
    /** 元件卸載：連同在飛的請求一起作廢。 */
    cancel() {
      seq += 1
      stop()
    },
  }
}
