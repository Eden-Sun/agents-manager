type Item = { kind: 'keys'; keys: string[] } | { kind: 'text'; text: string }

export const MAX_KEYS_PER_SEND = 64

export class KeyQueue {
  private pending: Item[] = []
  private sending = false
  private readonly send: (keys: string[]) => Promise<void>
  private readonly sendText?: (text: string) => Promise<void>
  private readonly onSettled?: (error: unknown | null) => void

  constructor(
    send: (keys: string[]) => Promise<void>,
    onSettled?: (error: unknown | null) => void,
    sendText?: (text: string) => Promise<void>,
  ) {
    this.send = send
    this.onSettled = onSettled
    this.sendText = sendText
  }

  push(keys: string[]): void {
    if (keys.length === 0) return
    this.pending.push({ kind: 'keys', keys })
    if (this.sending) return
    void this.drain()
  }

  pushText(text: string): void {
    if (!text || !this.sendText) return
    this.pending.push({ kind: 'text', text })
    if (this.sending) return
    void this.drain()
  }

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
        const first = this.pending.shift()!
        let job: Promise<void>
        if (first.kind === 'text') {
          job = this.sendText!(first.text)
        } else {
          const keys = first.keys.slice(0, MAX_KEYS_PER_SEND)
          if (first.keys.length > MAX_KEYS_PER_SEND) {
            this.pending.unshift({ kind: 'keys', keys: first.keys.slice(MAX_KEYS_PER_SEND) })
          }
          while (keys.length < MAX_KEYS_PER_SEND && this.pending[0]?.kind === 'keys') {
            const next = this.pending.shift() as { kind: 'keys'; keys: string[] }
            const room = MAX_KEYS_PER_SEND - keys.length
            keys.push(...next.keys.slice(0, room))
            if (next.keys.length > room) this.pending.unshift({ kind: 'keys', keys: next.keys.slice(room) })
          }
          job = this.send(keys)
        }
        try {
          await job
        } catch (e) {
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
