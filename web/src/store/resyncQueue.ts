/**
 * `resync` 幀＝「你漏了事件，整份重抓」。重抓進行中又來一個，不能丟：第一輪的 `refreshState` 可能早在
 * 第二個 resync 所指的變動之前就抓完，丟掉的話畫面停在舊資料，要等下一個事件才會動（#365）。
 * 所以進行中再來就記一筆「跑完再跑一次」，多個合併成一次。
 */
export function createResyncRunner(run: () => Promise<void>): () => void {
  let running = false
  let again = false
  const loop = async () => {
    running = true
    try {
      do {
        again = false
        // 這一輪出錯不能把排著的那次一起丟掉（#1698）：錯誤在這裡吞掉，`again` 照常決定要不要補跑。
        try {
          await run()
        } catch {
          /* run 自己負責回報；這裡只保證不漏跑排著的那次 */
        }
      } while (again)
    } finally {
      running = false
    }
  }
  return () => {
    if (running) {
      again = true
      return
    }
    void loop()
  }
}
