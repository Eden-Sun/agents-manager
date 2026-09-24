import test from 'node:test'
import assert from 'node:assert/strict'
import { pendingReducer, progressLabel, runUpload, uploadPercent } from './attachmentUpload'
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
