import test, { afterEach, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { installManualTimers } from '../testing/manualTimers'
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
  const gate = { hold: null as null | Promise<void>, fail: false, serverList: null as null | DraftWire[] }
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
    put: async (key, text, clientId) => {
      puts.push({ key, text, clientId })
      if (gate.hold) await gate.hold
      if (gate.fail) throw new Error('offline')
      const cur = server.get(key)
      if (cur && cur.text === text) return { rev: cur.rev }
      const rev = (cur?.rev ?? 0) + 1
      server.set(key, { text, rev })
      return { rev }
    },
    fetchAll: async () => gate.serverList ?? [...server].filter(([, v]) => v.text).map(([key, v]) => ({ key, ...v })),
  })
  const type = (key: string, text: string) => {
    if (text) local[key] = text
    else delete local[key]
    sync.localChange(key, text)
  }
  return { sync, local, focus, server, puts, gate, type }
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
