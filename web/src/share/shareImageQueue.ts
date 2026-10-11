/** 待開始的圖片下載／轉圖最多保留這麼多項，正在跑的那一項不算在內。 */
export const SHARE_IMAGE_PENDING_LIMIT = 24

export interface ImageWork<V> {
  promise: Promise<V>
  /** LIFO queue 中再次要求時，把尚未開始的工作移到最前面。 */
  promote(): void
  /** 尚未開始時取消；已開始的工作不會被中斷。 */
  cancel(): boolean
}

type Task = {
  key: string
  run: () => Promise<unknown>
  promise: Promise<unknown>
  resolve: (value: unknown) => void
  reject: (reason: unknown) => void
  state: 'pending' | 'active' | 'done' | 'cancelled'
}

/** 單 client 的圖片工作排程：一項執行、最多 24 項等待，新要求先做，同 key 共用同一工作。
 * 採 LIFO 不另量可視區：最新要求在當前工作結束後先跑；已開始的下載／轉圖不會中斷。 */
export class ShareImageWorkQueue {
  private readonly pending: Task[] = []
  private readonly tasks = new Map<string, Task>()
  private active: Task | null = null
  private readonly maxPending: number

  constructor(maxPending = SHARE_IMAGE_PENDING_LIMIT) {
    if (!Number.isInteger(maxPending) || maxPending < 0) throw new RangeError('maxPending must be a non-negative integer')
    this.maxPending = maxPending
  }

  get pendingCount(): number {
    return this.pending.length
  }

  get activeCount(): number {
    return this.active ? 1 : 0
  }

  schedule<V>(key: string, run: () => Promise<V>): ImageWork<V> {
    const hit = this.tasks.get(key)
    if (hit) {
      this.promoteTask(hit)
      return this.workFor<V>(hit)
    }

    let resolve!: (value: unknown) => void
    let reject!: (reason: unknown) => void
    const promise = new Promise<unknown>((yes, no) => {
      resolve = yes
      reject = no
    })
    const task: Task = { key, run, promise, resolve, reject, state: 'pending' }
    this.tasks.set(key, task)
    this.pending.push(task)
    if (this.pending.length > this.maxPending) {
      const oldest = this.pending.shift()!
      this.cancelTask(oldest, new Error('image_work_dropped'))
    }
    this.pump()
    return this.workFor<V>(task)
  }

  private workFor<V>(task: Task): ImageWork<V> {
    return {
      promise: task.promise as Promise<V>,
      promote: () => this.promoteTask(task),
      cancel: () => this.cancelTask(task, new Error('image_work_cancelled')),
    }
  }

  private promoteTask(task: Task) {
    if (task.state !== 'pending') return
    const index = this.pending.indexOf(task)
    if (index >= 0 && index !== this.pending.length - 1) {
      this.pending.splice(index, 1)
      this.pending.push(task)
    }
  }

  private cancelTask(task: Task, reason: Error): boolean {
    if (task.state !== 'pending') return false
    const index = this.pending.indexOf(task)
    if (index < 0) return false
    this.pending.splice(index, 1)
    task.state = 'cancelled'
    if (this.tasks.get(task.key) === task) this.tasks.delete(task.key)
    task.reject(reason)
    return true
  }

  private pump() {
    if (this.active) return
    const task = this.pending.pop()
    if (!task) return
    task.state = 'active'
    this.active = task
    void Promise.resolve()
      .then(task.run)
      .then(task.resolve, task.reject)
      .finally(() => {
        task.state = 'done'
        if (this.tasks.get(task.key) === task) this.tasks.delete(task.key)
        if (this.active === task) this.active = null
        this.pump()
      })
  }
}
