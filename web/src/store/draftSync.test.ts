import test, { afterEach, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { installManualTimers } from '../testing/manualTimers'
import { ApiError } from '../api/types'
import { DraftSync, type DraftWire } from './draftSync'

// debounce／重試是 `setTimeout`：換成手動時鐘，`tick(ms)` 才是「過了 ms 毫秒」而不是「睡 ms 毫秒然後祈禱」。
// 牆鐘版本在高負載下會因為兩次 `tick(10)` 被拖長超過 30ms 的 debounce 而多送一次 PUT。
let timers!: ReturnType<typeof installManualTimers>
beforeEach(() => {
  timers = installManualTimers()
})
afterEach(() => timers.restore())

/** 假的 daemon＋輸入框：用手動時鐘推進 debounce，PUT 可以卡住或失敗。 */
function rig(opts: { debounceMs?: number } = {}) {
  const local: Record<string, string> = {}
  const focus = new Set<string>()
  const server = new Map<string, { text: string; rev: number }>()
  const puts: { key: string; text: string; clientId: string }[] = []
  const rejected: string[] = []
  const cleared: string[] = []
  const gate = {
    hold: null as null | Promise<void>,
    /** daemon 已經 commit，只把 HTTP 回應延後（PUT 回應晚到的時序）。 */
    replyHold: null as null | Promise<void>,
    fetchHold: null as null | Promise<void>,
    fail: false,
    failWith: null as unknown,
    serverList: null as null | DraftWire[],
    onCommit: null as null | ((key: string, text: string, rev: number) => void),
  }
  const sync = new DraftSync({
    clientId: 'me',
    debounceMs: opts.debounceMs ?? 400,
    retryMs: 3000,
    read: (k) => local[k] ?? '',
    write: (k, t) => {
      if (t) local[k] = t
      else delete local[k]
    },
    focused: (k) => focus.has(k),
    keys: () => Object.keys(local),
    onRejected: (key) => rejected.push(key),
    onRejectedCleared: (key) => cleared.push(key),
    put: async (key, text, clientId) => {
      puts.push({ key, text, clientId })
      if (gate.hold) await gate.hold
      if (gate.failWith) throw gate.failWith
      if (gate.fail) throw new Error('offline')
      const cur = server.get(key)
      if (cur && cur.text === text) return { rev: cur.rev }
      const rev = (cur?.rev ?? 0) + 1
      server.set(key, { text, rev })
      gate.onCommit?.(key, text, rev)
      if (gate.replyHold) await gate.replyHold
      return { rev }
    },
    fetchAll: async () => {
      const list = gate.serverList ?? [...server].filter(([, v]) => v.text).map(([key, v]) => ({ key, ...v }))
      if (gate.fetchHold) await gate.fetchHold
      return list
    },
  })
  const type = (key: string, text: string) => {
    if (text) local[key] = text
    else delete local[key]
    sync.localChange(key, text)
  }
  return { sync, local, focus, server, puts, gate, type, rejected, cleared }
}

/** 虛擬時間過 `ms`，並讓 PUT 的 promise 鏈跑完。 */
const settleMicrotasks = () => new Promise<void>((resolve) => setImmediate(resolve))
const tick = async (ms: number) => {
  await settleMicrotasks() // 先讓剛 resolve 的 PUT 跑完，它排的 debounce 才進得了這個窗口
  await timers.clock.advance(ms)
  await settleMicrotasks()
}

test('打字 debounce：連打只送最後一次', async () => {
  const r = rig({ debounceMs: 30 })
  r.type('bot:a', 'h')
  await tick(10)
  r.type('bot:a', 'he')
  await tick(10)
  r.type('bot:a', 'hel')
  assert.equal(r.puts.length, 0)
  await tick(60)
  assert.deepEqual(r.puts.map((p) => p.text), ['hel'])
  assert.equal(r.puts[0].clientId, 'me')
  assert.equal(r.sync.isDirty('bot:a'), false)
  assert.equal(r.sync.knownRev('bot:a'), 1)
})

test('清空（送出）不等 debounce，馬上送', async () => {
  const r = rig({ debounceMs: 10_000 })
  r.type('bot:a', 'x')
  r.type('bot:a', '')
  await tick(5)
  assert.deepEqual(r.puts.map((p) => p.text), [''])
})

test('別人的草稿：沒焦點就套用', () => {
  const r = rig()
  r.sync.remote({ key: 'bot:a', text: '來自手機', rev: 1, client_id: 'phone' })
  assert.equal(r.local['bot:a'], '來自手機')
  r.sync.remote({ key: 'bot:a', text: '', rev: 2, client_id: 'phone' })
  assert.equal(r.local['bot:a'], undefined, '對方送出清空，這裡也清')
})

test('有焦點而且有未同步的修改：不覆蓋，等本機的 PUT 贏', async () => {
  const r = rig({ debounceMs: 20 })
  r.focus.add('bot:a')
  r.type('bot:a', '我正在打')
  r.sync.remote({ key: 'bot:a', text: '別人的', rev: 5, client_id: 'phone' })
  assert.equal(r.local['bot:a'], '我正在打')
  await tick(50)
  assert.deepEqual(r.puts.map((p) => p.text), ['我正在打'], '本機那次照送（最後寫入者贏）')
  assert.equal(r.local['bot:a'], '我正在打')
  assert.ok(r.sync.knownRev('bot:a') >= 5)
})

test('有焦點但本機沒有未同步的修改：照樣套用', () => {
  const r = rig()
  r.focus.add('bot:a')
  r.sync.remote({ key: 'bot:a', text: '別人的', rev: 1, client_id: 'phone' })
  assert.equal(r.local['bot:a'], '別人的')
})

test('沒焦點但有未同步的修改：套用別人的，本機排著的 PUT 取消', async () => {
  const r = rig({ debounceMs: 20 })
  r.type('bot:a', '舊的字')
  r.sync.remote({ key: 'bot:a', text: '別人的', rev: 3, client_id: 'phone' })
  assert.equal(r.local['bot:a'], '別人的')
  await tick(50)
  assert.equal(r.puts.length, 0)
})

test('在飛 PUT 較晚提交時，本機套回它確認寫入的版本；自己的回音先到', async () => {
  const r = rig({ debounceMs: 10 })
  let release = () => {}
  r.gate.hold = new Promise<void>((res) => (release = res))
  r.server.set('bot:a', { text: 'phone-B', rev: 2 })
  r.type('bot:a', 'desktop-A')
  await tick(30)
  assert.deepEqual(r.puts.map((p) => p.text), ['desktop-A'])

  r.sync.remote({ key: 'bot:a', text: 'phone-B', rev: 2, client_id: 'phone' })
  assert.equal(r.local['bot:a'], 'phone-B')
  r.gate.onCommit = (key, text, rev) => r.sync.remote({ key, text, rev, client_id: 'me' })
  r.gate.hold = null
  release()
  await tick(30)

  assert.equal(r.local['bot:a'], 'desktop-A')
  assert.deepEqual(r.server.get('bot:a'), { text: 'desktop-A', rev: 3 })
  assert.equal(r.sync.knownRev('bot:a'), 3)
  assert.equal(r.sync.isDirty('bot:a'), false)
  await r.sync.load()
  assert.equal(r.local['bot:a'], 'desktop-A', '同 rev 的 load 不得留下不同內容')
  assert.equal(r.puts.length, 1)
})

test('在飛 PUT 較晚提交時，本機套回它確認寫入的版本；自己的回音晚到', async () => {
  const r = rig({ debounceMs: 10 })
  let release = () => {}
  r.gate.hold = new Promise<void>((res) => (release = res))
  r.server.set('bot:a', { text: 'phone-B', rev: 2 })
  r.type('bot:a', 'desktop-A')
  await tick(30)
  r.sync.remote({ key: 'bot:a', text: 'phone-B', rev: 2, client_id: 'phone' })

  r.gate.hold = null
  release()
  await tick(30)
  assert.equal(r.local['bot:a'], 'desktop-A')
  assert.equal(r.sync.knownRev('bot:a'), 3)
  r.sync.remote({ key: 'bot:a', text: 'desktop-A', rev: 3, client_id: 'me' })
  assert.equal(r.local['bot:a'], 'desktop-A')
  assert.equal(r.sync.isDirty('bot:a'), false)
  assert.equal(r.puts.length, 1)
})

test('在飛 PUT 較晚提交時，遠端清空也不會讓同 rev 的畫面與 daemon 分歧', async () => {
  const r = rig({ debounceMs: 10 })
  let release = () => {}
  r.gate.hold = new Promise<void>((res) => (release = res))
  r.server.set('bot:a', { text: 'phone-B', rev: 2 })
  r.type('bot:a', 'desktop-A')
  await tick(30)
  r.sync.remote({ key: 'bot:a', text: '', rev: 2, client_id: 'phone' })
  assert.equal(r.local['bot:a'], undefined)

  r.gate.hold = null
  release()
  await tick(30)

  assert.equal(r.local['bot:a'], 'desktop-A')
  assert.deepEqual(r.server.get('bot:a'), { text: 'desktop-A', rev: 3 })
  assert.equal(r.sync.knownRev('bot:a'), 3)
  assert.equal(r.sync.isDirty('bot:a'), false)
  await r.sync.load()
  assert.equal(r.local['bot:a'], 'desktop-A')
  assert.equal(r.puts.length, 1)
})

/**
 * 焦點在輸入框、本機 A 已被 daemon commit 為 rev1，但 PUT 回應還沒回來；這段時間 daemon 又提交了較新的遠端版本，
 * 輸入框因為有焦點而略過它。PUT 回應之後，本機不能把 A 當成已同步（daemon 其實是遠端那版）。
 */
async function focusedRemoteSkippedBeforeReply(
  remote: { text: string; rev: number },
  opts: { echoFirst?: boolean; blurBeforeReply?: boolean } = {},
) {
  const r = rig({ debounceMs: 10 })
  let releaseReply = () => {}
  r.gate.replyHold = new Promise<void>((res) => (releaseReply = res))
  r.focus.add('bot:a')
  r.type('bot:a', 'desktop-A')
  await tick(30)
  assert.deepEqual(r.server.get('bot:a'), { text: 'desktop-A', rev: 1 }, 'PUT(A) 已 commit，只差回應')
  if (opts.echoFirst) r.sync.remote({ key: 'bot:a', text: 'desktop-A', rev: 1, client_id: 'me' })
  r.server.set('bot:a', remote)
  r.sync.remote({ key: 'bot:a', ...remote, client_id: 'phone' })
  assert.equal(r.local['bot:a'], 'desktop-A', '有焦點且有未同步修改：略過遠端')
  if (opts.blurBeforeReply) r.focus.delete('bot:a')
  r.gate.replyHold = null
  releaseReply()
  await tick(30)
  return r
}

function assertConvergedOnDesktopA(r: ReturnType<typeof rig>) {
  assert.equal(r.local['bot:a'], 'desktop-A')
  assert.deepEqual(r.server.get('bot:a'), { text: 'desktop-A', rev: 3 }, '本機 A 要被再送一次，daemon 與畫面一致')
  assert.equal(r.sync.knownRev('bot:a'), 3)
  assert.equal(r.sync.isDirty('bot:a'), false)
  assert.deepEqual(r.puts.map((p) => p.text), ['desktop-A', 'desktop-A'])
}

test('有焦點略過較新的遠端版本、PUT 回應晚到：本機 A 不能被當成已同步，再送一次（#1665 回歸）', async () => {
  const r = await focusedRemoteSkippedBeforeReply({ text: 'phone-B', rev: 2 })
  assertConvergedOnDesktopA(r)
  await r.sync.load()
  assert.equal(r.local['bot:a'], 'desktop-A', '同 rev 的 load 不得留下不同內容')
})

test('有焦點略過的遠端是清空：回應晚到後仍收斂到本機 A（#1665 回歸）', async () => {
  const r = await focusedRemoteSkippedBeforeReply({ text: '', rev: 2 })
  assertConvergedOnDesktopA(r)
})

test('有焦點略過較新的遠端版本，自己的回音先到：仍收斂到本機 A（#1665 回歸）', async () => {
  const r = await focusedRemoteSkippedBeforeReply({ text: 'phone-B', rev: 2 }, { echoFirst: true })
  assertConvergedOnDesktopA(r)
})

test('有焦點略過較新的遠端版本，回應前焦點離開：仍收斂到本機 A（#1665 回歸）', async () => {
  const r = await focusedRemoteSkippedBeforeReply({ text: 'phone-B', rev: 2 }, { blurBeforeReply: true })
  assertConvergedOnDesktopA(r)
})

test('在飛 PUT 失敗時保留已接受的遠端版本，不排空的重試', async () => {
  const r = rig({ debounceMs: 10 })
  let release = () => {}
  r.gate.hold = new Promise<void>((res) => (release = res))
  r.gate.failWith = new TypeError('Failed to fetch')
  r.server.set('bot:a', { text: 'phone-B', rev: 2 })
  r.type('bot:a', 'desktop-A')
  await tick(30)
  r.sync.remote({ key: 'bot:a', text: 'phone-B', rev: 2, client_id: 'phone' })

  r.gate.hold = null
  release()
  await tick(30)
  await tick(30_000)

  assert.equal(r.local['bot:a'], 'phone-B')
  assert.deepEqual(r.server.get('bot:a'), { text: 'phone-B', rev: 2 })
  assert.equal(r.sync.knownRev('bot:a'), 2)
  assert.equal(r.sync.isDirty('bot:a'), false)
  assert.equal(r.puts.length, 1)
})

test('PUT 回應前已接受更高 rev 時保留較新的遠端草稿', async () => {
  const r = rig({ debounceMs: 10 })
  let release = () => {}
  r.gate.hold = new Promise<void>((res) => (release = res))
  r.server.set('bot:a', { text: 'phone-B', rev: 2 })
  r.type('bot:a', 'desktop-A')
  await tick(30)
  r.sync.remote({ key: 'bot:a', text: 'phone-B', rev: 2, client_id: 'phone' })
  r.gate.onCommit = (key, _text) => {
    r.server.set(key, { text: 'phone-C', rev: 4 })
    r.sync.remote({ key, text: 'phone-C', rev: 4, client_id: 'another-phone' })
  }

  r.gate.hold = null
  release()
  await tick(30)

  assert.equal(r.local['bot:a'], 'phone-C')
  assert.deepEqual(r.server.get('bot:a'), { text: 'phone-C', rev: 4 })
  assert.equal(r.sync.knownRev('bot:a'), 4)
  assert.equal(r.sync.isDirty('bot:a'), false)
  assert.equal(r.puts.length, 1, '不應把較舊版本再排隊寫回')
})

test('forget 取消在飛寫入後，成功回應不會把輸入框草稿復活', async () => {
  const r = rig({ debounceMs: 10 })
  let release = () => {}
  r.gate.hold = new Promise<void>((res) => (release = res))
  r.type('bot:a', 'old')
  await tick(30)
  r.sync.forget('bot:a')
  delete r.local['bot:a']

  r.gate.hold = null
  release()
  await tick(30)

  assert.equal(r.local['bot:a'], undefined)
  assert.equal(r.sync.isDirty('bot:a'), false)
})

test('自己的回音不重套：只記 rev', async () => {
  const r = rig({ debounceMs: 10 })
  r.type('bot:a', 'abc')
  await tick(30)
  r.type('bot:a', 'abcd') // 回音到之前又多打了一個字
  r.sync.remote({ key: 'bot:a', text: 'abc', rev: 1, client_id: 'me' })
  assert.equal(r.local['bot:a'], 'abcd')
  assert.equal(r.sync.knownRev('bot:a'), 1)
})

test('rev 不大於已知的事件是舊事件：丟掉', () => {
  const r = rig()
  r.sync.remote({ key: 'bot:a', text: 'new', rev: 4, client_id: 'phone' })
  r.sync.remote({ key: 'bot:a', text: 'old', rev: 3, client_id: 'phone' })
  r.sync.remote({ key: 'bot:a', text: 'dup', rev: 4, client_id: 'phone' })
  assert.equal(r.local['bot:a'], 'new')
})

test('PUT 在飛時本機又改了：回來之後再送一次，順序不亂', async () => {
  const r = rig({ debounceMs: 10 })
  let release = () => {}
  r.gate.hold = new Promise<void>((res) => (release = res))
  r.type('bot:a', 'one')
  await tick(30)
  r.type('bot:a', 'one two')
  await tick(30)
  assert.deepEqual(r.puts.map((p) => p.text), ['one'], '前一個沒回來，不會有第二個並行')
  r.gate.hold = null
  release()
  await tick(60)
  assert.deepEqual(r.puts.map((p) => p.text), ['one', 'one two'])
  assert.equal(r.sync.isDirty('bot:a'), false)
})

test('PUT 失敗：維持 dirty、之後重試；重連 load 會補送', async () => {
  const r = rig({ debounceMs: 5 })
  r.gate.fail = true
  r.type('bot:a', 'offline')
  await tick(20)
  assert.equal(r.sync.isDirty('bot:a'), true)
  r.gate.fail = false
  await r.sync.load() // 重連：先拉（本機 dirty 不被蓋），再補送
  await tick(20)
  assert.equal(r.local['bot:a'], 'offline')
  assert.equal(r.server.get('bot:a')?.text, 'offline')
  assert.equal(r.sync.isDirty('bot:a'), false)
})

test('load：套用 daemon 的草稿；daemon 沒有而本機乾淨的＝已被刪，跟著清', async () => {
  const r = rig()
  r.local['bot:gone'] = '已被別處送出'
  r.server.set('bot:a', { text: '桌機打的', rev: 7 })
  await r.sync.load()
  assert.equal(r.local['bot:a'], '桌機打的')
  assert.equal(r.local['bot:gone'], undefined)
  assert.equal(r.sync.knownRev('bot:a'), 7)
})

test('load：本機有未同步修改的不被 daemon 的版本蓋掉也不被清掉', async () => {
  const r = rig({ debounceMs: 10_000 })
  r.focus.add('bot:a')
  r.type('bot:a', '本機新打的')
  r.server.set('bot:a', { text: '舊的', rev: 2 })
  r.type('bot:b', '只在本機')
  await r.sync.load()
  await tick(10)
  assert.equal(r.local['bot:a'], '本機新打的')
  assert.equal(r.local['bot:b'], '只在本機')
  assert.deepEqual(r.puts.map((p) => p.text).sort(), ['只在本機', '本機新打的'].sort(), 'load 完把 dirty 的送出去')
})

test('load 的回應比本機剛送出的寫入舊：不能當成 daemon 刪了它', async () => {
  const r = rig({ debounceMs: 5 })
  r.gate.serverList = [] // GET 在 PUT 落地之前拍的快照
  const loading = r.sync.load()
  r.type('bot:a', 'fresh')
  await tick(20) // PUT 完成，dirty 已清
  await loading
  assert.equal(r.local['bot:a'], 'fresh')
})

test('forget：bot 被刪後不再送', async () => {
  const r = rig({ debounceMs: 10 })
  r.type('bot:a', 'x')
  r.sync.forget('bot:a')
  await tick(30)
  assert.equal(r.puts.length, 0)
})

test('草稿太大（400）：不重送、不當成 daemon 沒有，本機改短再照常同步（#894）', async () => {
  const r = rig({ debounceMs: 10 })
  r.gate.failWith = new ApiError(400, { error: 'draft_too_large' }, 'draft_too_large')
  r.type('bot:a', 'x'.repeat(300_000))
  await tick(30)
  await tick(30_000)
  assert.equal(r.puts.length, 1, '被拒絕後不再每 3 秒重送')
  assert.equal(r.sync.isDirty('bot:a'), false)
  assert.deepEqual(r.rejected, ['bot:a'])

  await r.sync.load() // fetchAll 回 []：本機那份大草稿不能被清成 ''
  assert.equal(r.local['bot:a']?.length, 300_000)

  r.gate.failWith = null
  r.type('bot:a', '短了')
  await tick(30)
  assert.deepEqual(r.puts.map((p) => p.text).slice(-1), ['短了'])
  assert.equal(r.server.get('bot:a')?.text, '短了')
  assert.equal(r.sync.isDirty('bot:a'), false)
})

test('被拒的草稿：打字改短但還沒送進 daemon 前提示不收，送成功才通知收掉（#1656）', async () => {
  const r = rig({ debounceMs: 10 })
  r.gate.failWith = new ApiError(400, { error: 'draft_too_large', max_bytes: 262144 }, 'draft_too_large')
  r.type('bot:a', 'x'.repeat(300_000))
  await tick(30)
  assert.deepEqual(r.rejected, ['bot:a'])
  assert.deepEqual(r.cleared, [], '還是超過上限的時候不收')
  r.gate.failWith = null
  r.type('bot:a', '短了')
  assert.deepEqual(r.cleared, [], '打字當下還沒送進 daemon，不收')
  await tick(30)
  assert.deepEqual(r.cleared, ['bot:a'])
})

test('被拒的草稿：又被拒（仍超過上限）不收、再次通知；沒被拒過的草稿送出不通知', async () => {
  const r = rig({ debounceMs: 10 })
  r.gate.failWith = new ApiError(400, { error: 'draft_too_large', max_bytes: 262144 }, 'draft_too_large')
  r.type('bot:a', 'x'.repeat(300_000))
  await tick(30)
  r.type('bot:a', 'y'.repeat(300_000))
  await tick(30)
  assert.deepEqual(r.rejected, ['bot:a', 'bot:a'])
  assert.deepEqual(r.cleared, [])
  r.gate.failWith = null
  r.type('bot:b', '正常')
  await tick(30)
  assert.deepEqual(r.cleared, [], '沒有提示過的 key 不通知')
})

test('被拒的草稿：別台寫入新版本、或 bot 被刪（forget），提示也跟著收掉', async () => {
  const r = rig({ debounceMs: 10 })
  r.gate.failWith = new ApiError(400, { error: 'draft_too_large', max_bytes: 262144 }, 'draft_too_large')
  r.type('bot:a', 'x'.repeat(300_000))
  r.type('bot:b', 'x'.repeat(300_000))
  await tick(30)
  assert.deepEqual(r.rejected, ['bot:a', 'bot:b'])
  r.sync.remote({ key: 'bot:a', text: '別台的', rev: 5, client_id: 'other' })
  assert.deepEqual(r.cleared, ['bot:a'])
  r.sync.forget('bot:b')
  assert.deepEqual(r.cleared, ['bot:a', 'bot:b'])
  r.sync.forget('bot:b')
  assert.deepEqual(r.cleared, ['bot:a', 'bot:b'], '已經收過的不重複通知')
})

test('413 也一樣不重送', async () => {
  const r = rig({ debounceMs: 10 })
  r.gate.failWith = new ApiError(413, {}, 'too large')
  r.type('bot:a', 'big')
  await tick(30)
  await tick(30_000)
  assert.equal(r.puts.length, 1)
  assert.deepEqual(r.rejected, ['bot:a'])
})

test('PUT 被拒的同時本機又改了：新的那段照樣排隊送出，不能因為舊字被拒就跟著丟掉', async () => {
  const r = rig({ debounceMs: 10 })
  let release = () => {}
  r.gate.hold = new Promise<void>((res) => (release = res))
  r.type('bot:a', 'x'.repeat(300_000))
  await tick(30) // 大字的 PUT 在飛、卡住
  r.type('bot:a', '短了') // 打字期間又改短：debounce 已排好
  r.gate.failWith = new ApiError(400, { error: 'draft_too_large' }, 'draft_too_large')
  release() // 大字被 daemon 拒絕
  await settleMicrotasks()
  r.gate.hold = null
  r.gate.failWith = null
  await tick(30)
  assert.equal(r.puts.length, 2, '短的那段要送出去，不能被舊字的拒絕蓋掉')
  assert.equal(r.puts[1].text, '短了')
  assert.equal(r.server.get('bot:a')?.text, '短了')
  assert.equal(r.sync.isDirty('bot:a'), false)
})

test('斷線（TypeError）照舊 3 秒後重送', async () => {
  const r = rig({ debounceMs: 10 })
  r.gate.failWith = new TypeError('Failed to fetch')
  r.type('bot:a', 'x')
  await tick(30)
  assert.equal(r.puts.length, 1)
  await tick(3000)
  assert.equal(r.puts.length, 2)
  assert.equal(r.sync.isDirty('bot:a'), true)
  assert.deepEqual(r.rejected, [])
})

test('load 在飛時收到別台的新草稿：舊快照回來不能把它清掉', async () => {
  const r = rig()
  let release!: () => void
  r.gate.fetchHold = new Promise<void>((res) => {
    release = res
  })
  const loading = r.sync.load()
  r.sync.remote({ key: 'bot:b1', text: 'from phone', rev: 1, client_id: 'other' })
  assert.equal(r.local['bot:b1'], 'from phone')
  release()
  await loading
  assert.equal(r.local['bot:b1'], 'from phone')
})

test('load 之前就有、daemon 真的沒有的乾淨草稿照舊清掉', async () => {
  const r = rig()
  r.local['bot:old'] = 'stale'
  r.gate.serverList = []
  await r.sync.load()
  assert.equal(r.local['bot:old'], undefined)
})

