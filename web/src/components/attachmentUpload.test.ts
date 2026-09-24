import test from 'node:test'
import assert from 'node:assert/strict'
import { admitFile, pendingReducer, progressGate, progressLabel, runUpload, uploadPercent } from './attachmentUpload'
import type { Pending, PendingAction, UploadDeps } from './attachmentUpload'
import type { UploadOptions } from '../api/transport'
import { abortError } from '../api/transport'
import { MAX_BYTES } from '../store/shelf'

const MB = 1024 * 1024

const item = (over: Partial<Pending> = {}): Pending => ({
  key: 'a1', fp: 'f', name: 'big.zip', size: 12.6 * MB, isImage: false, previewUrl: '',
  compressing: false, loaded: 0, id: null, error: null, retryable: true, ...over,
})

/** 一個還沒回應的上傳：測試手動推進度、成功、失敗。 */
function harness(compressTo?: File) {
  const actions: PendingAction[] = []
  const errors: string[] = []
  let items: Pending[] = [item()]
  let opts: UploadOptions | null = null
  let settle: { resolve: (id: string) => void; reject: (e: unknown) => void } | null = null
  const deps: UploadDeps = {
    compress: async (f) => compressTo ?? f,
    upload: (_f, o) => {
      opts = o
      return new Promise((resolve, reject) => {
        settle = { resolve: (id) => resolve({ id, name: 'big.zip', mime: '', size: 0, path: '' }), reject }
        // 跟 XHR 一樣：signal 中止就以 AbortError 收掉。
        o.signal?.addEventListener('abort', () => reject(abortError()), { once: true })
      })
    },
    dispatch: (a) => {
      actions.push(a)
      items = pendingReducer(items, a)
    },
    onPrepared: () => {},
    onError: (_n, m) => errors.push(m),
    formatSize: (b) => `${b}B`,
  }
  return {
    deps, actions, errors,
    get items() { return items },
    get opts() { return opts },
    get settle() { return settle! },
  }
}

const tick = () => new Promise((r) => setTimeout(r, 0))

test('進度事件推進卡片：已傳位元組跟著 onProgress 走，送完拿到 id', async () => {
  const h = harness()
  const ctl = new AbortController()
  const done = runUpload(h.deps, 'a1', new File(['x'], 'big.zip'), false, ctl.signal)
  await tick()
  assert.ok(h.opts?.onProgress, '要把 onProgress 交給 transport')
  h.opts!.onProgress!(3.2 * MB, 12.6 * MB)
  assert.equal(h.items[0].loaded, 3.2 * MB)
  assert.equal(progressLabel(h.items[0].loaded, h.items[0].size), '3.2 / 12.6 MB · 25%')
  h.settle.resolve('att1')
  await done
  assert.equal(h.items[0].id, 'att1')
  assert.equal(h.errors.length, 0)
})

test('進度重繪節流：50 MB 只在卡片那行字會變時才 dispatch，而且一階都沒少（issue #438）', async () => {
  const h = harness()
  const ctl = new AbortController()
  const done = runUpload(h.deps, 'a1', new File(['x'], 'big.zip'), false, ctl.signal)
  await tick()
  const total = 50 * MB
  // XHR 的上傳進度事件最密約每 50 ms 一次；5 分鐘的上傳就是這個量級。
  const events = 6000
  const raw: number[] = []
  for (let i = 1; i <= events; i++) raw.push(Math.round((total * i) / events))
  for (const loaded of raw) h.opts!.onProgress!(loaded, total)

  const drawn = h.actions.filter((a) => a.type === 'progress').map((a) => (a as { loaded: number }).loaded)
  // 節流前是每個事件一次；節流後只剩文字真的會變的次數（0.1 MB 與 1% 裡較細的那個，約 500 階）。
  assert.ok(drawn.length < events / 8, `${events} 個事件應該被節流到幾百次，實際 ${drawn.length}`)

  // **一階都不能少**：節流不是抽樣——原始事件流會畫出來的每一種文字都要真的被送出去過，
  // 否則卡片會跳號（例如從 12% 直接跳到 14%）。
  const seen = new Set(drawn.map((l) => progressLabel(l, total)))
  const every = new Set(raw.map((l) => progressLabel(l, total)))
  assert.deepEqual([...seen].sort(), [...every].sort(), '看得到的每一階都要有對應的 dispatch')
  // 而且順序是遞增的，不會把舊的值蓋回去。
  assert.deepEqual(drawn, [...drawn].sort((a, b) => a - b))

  h.settle.resolve('att1')
  await done
  assert.equal(h.items[0].id, 'att1')
})

test('節流閘門本身：同一行字只放行一次，字變了才再放行（issue #438）', () => {
  const gate = progressGate()
  const total = 50 * MB
  assert.equal(gate(0, total), true, '第一次一定要畫')
  assert.equal(gate(1024, total), false, '1 KB 看不出來')
  assert.equal(gate(2048, total), false)
  assert.equal(gate(0.1 * MB, total), true, '0.1 MB 就是文字的最小刻度')
  assert.equal(gate(0.1 * MB + 1, total), false)
  // 每一張卡片各自一個閘門，不會互相影響。
  assert.equal(progressGate()(0.1 * MB, total), true)
})

test('× 中止：signal 真的交到 transport、被 abort，卡片不記失敗也不跳通知', async () => {
  const h = harness()
  const ctl = new AbortController()
  const done = runUpload(h.deps, 'a1', new File(['x'], 'big.zip'), false, ctl.signal)
  await tick()
  assert.equal(h.opts?.signal, ctl.signal)
  ctl.abort()
  assert.equal(h.opts?.signal?.aborted, true)
  await done
  assert.equal(h.items[0].error, null)
  assert.equal(h.errors.length, 0)
  assert.ok(!h.actions.some((a) => a.type === 'failed'))
})

test('失敗：卡片記原因、可以重試，並通知一次', async () => {
  const h = harness()
  const done = runUpload(h.deps, 'a1', new File(['x'], 'big.zip'), false, new AbortController().signal)
  await tick()
  h.settle.reject(new Error('連線中斷，上傳沒有完成'))
  await done
  assert.equal(h.items[0].error, '連線中斷，上傳沒有完成')
  assert.equal(h.items[0].retryable, true)
  assert.deepEqual(h.errors, ['連線中斷，上傳沒有完成'])
  // 重試把錯誤與進度歸零、又回到「上傳中」。
  const again = pendingReducer(h.items, { type: 'retry', key: 'a1' })
  assert.equal(again[0].error, null)
  assert.equal(again[0].loaded, 0)
})

test('壓完還超過上限：失敗但不給重試（再傳一次也一樣）', async () => {
  const huge = { size: MAX_BYTES + 1, name: 'huge.jpg' } as File
  const h = harness(huge)
  await runUpload(h.deps, 'a1', new File(['x'], 'huge.jpg'), true, new AbortController().signal)
  assert.equal(h.items[0].retryable, false)
  assert.match(h.items[0].error ?? '', /上限/)
  assert.equal(h.opts, null, '超過上限不該開始上傳')
})

test('壓縮中 → 開始上傳時才清掉「壓縮中」', async () => {
  const h = harness()
  h.deps.dispatch({ type: 'add', item: item({ key: 'b', compressing: true }) })
  assert.equal(h.items[1].compressing, true)
  h.deps.dispatch({ type: 'uploading', key: 'b' })
  assert.equal(h.items[1].compressing, false)
})

test('百分比往下取整、單位跟著總大小', () => {
  assert.equal(uploadPercent(996, 1000), 99)
  assert.equal(uploadPercent(5, 0), 0)
  assert.equal(progressLabel(300 * 1024, 800 * 1024), '300 / 800 KB · 37%')
  assert.equal(progressLabel(10, 100), '10 / 100 B · 10%')
})

test('擋下來的大檔不記指紋：同一個檔再拖一次照樣講一次「超過上限」（issue #497）', () => {
  const seen = new Set<string>()
  const big = 60 * MB
  assert.equal(admitFile(seen, 'big.zip|1', big, false), 'too-large')
  // 擋下來的那一次沒有卡片、沒有 × 可以按；指紋要是記了，第二次就變成默默略過。
  assert.equal(seen.size, 0)
  assert.equal(admitFile(seen, 'big.zip|1', big, false), 'too-large')
})

test('收下的才記指紋，第二次同一個檔算重複', () => {
  const seen = new Set<string>()
  assert.equal(admitFile(seen, 'a.png|1', 3 * MB, false), 'accept')
  assert.deepEqual([...seen], ['a.png|1'])
  assert.equal(admitFile(seen, 'a.png|1', 3 * MB, false), 'duplicate')
  assert.equal(admitFile(seen, 'b.png|1', 3 * MB, false), 'accept')
})

test('圖片超過上限先收下：壓完才比上限（壓不下去的由 runUpload 記失敗）', () => {
  assert.equal(admitFile(new Set(), 'huge.png|1', MAX_BYTES + 1, true), 'accept')
  assert.equal(admitFile(new Set(), 'huge.zip|1', MAX_BYTES + 1, false), 'too-large')
  assert.equal(admitFile(new Set(), 'edge.zip|1', MAX_BYTES, false), 'accept')
})
