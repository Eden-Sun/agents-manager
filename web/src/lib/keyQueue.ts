/**
 * 按鍵的送出佇列：**依序、合批、不重疊**。
 *
 * 一鍵一個 POST 會同時在路上，抵達順序不保證——打 `ls` 可能變成 `sl`。所以同一時間只有一個
 * 請求在飛，飛的期間按的鍵累積起來，下一輪一次送完。鍵盤同步模式下這不是最佳化，是正確性：
 * 使用者是照順序按的，pane 收到的順序就必須一樣。
 */
/** 排在佇列裡的一項：一串鍵，或一段貼上的文字（貼上不能拆成鍵——換行會變成 Enter 直接執行）。 */
type Item = { kind: 'keys'; keys: string[] } | { kind: 'text'; text: string }

export class KeyQueue {
  private pending: Item[] = []
  private sending = false
  private readonly send: (keys: string[]) => Promise<void>
  private readonly sendText?: (text: string) => Promise<void>
  private readonly onSettled?: (error: unknown | null) => void

  // 參數屬性（`private readonly send: …` 直接寫在 constructor 上）在 `erasableSyntaxOnly` 下不能用。
  constructor(
    send: (keys: string[]) => Promise<void>,
    onSettled?: (error: unknown | null) => void,
    sendText?: (text: string) => Promise<void>,
  ) {
    this.send = send
    this.onSettled = onSettled
    this.sendText = sendText
  }

  /** 排進去並（必要時）開始送。不等待：呼叫端是 keydown handler，不能被網路拖住。 */
  push(keys: string[]): void {
    if (keys.length === 0) return
    this.pending.push({ kind: 'keys', keys })
    if (this.sending) return
    void this.drain()
  }

  /** 貼上：走同一個佇列才保得住「打一半貼一段」的順序。沒給 `sendText` 就丟掉（呼叫端不支援貼上）。 */
  pushText(text: string): void {
    if (!text || !this.sendText) return
    this.pending.push({ kind: 'text', text })
    if (this.sending) return
    void this.drain()
  }

  /** 還沒送出去的鍵（換 pane 時要丟掉：那是給上一個 pane 的輸入）。 */
  clear(): void {
    this.pending = []
  }

  get inFlight(): boolean {
    return this.sending
  }

  private async drain(): Promise<void> {
    this.sending = true
    let error: unknown | null = null
    try {
      while (this.pending.length) {
        // 連著的鍵合成一批；碰到貼上就先斷開（順序比批次大小重要）。
        const first = this.pending.shift()!
        let job: Promise<void>
        if (first.kind === 'text') {
          job = this.sendText!(first.text)
        } else {
          const keys = [...first.keys]
          while (this.pending[0]?.kind === 'keys') {
            keys.push(...(this.pending.shift() as { kind: 'keys'; keys: string[] }).keys)
          }
          job = this.send(keys)
        }
        try {
          await job
        } catch (e) {
          // 這一批沒送成功就停：後面的鍵接在一段沒進去的輸入後面，多半只會讓畫面更亂。
          // 剩下的一起丟掉，讓使用者看到錯誤自己決定要不要重打。
          error = e
          this.pending = []
          break
        }
      }
    } finally {
      this.sending = false
      this.onSettled?.(error)
    }
  }
}
