/**
 * xterm onData → herdr text／keys 操作（xtermInput.ts）。鍵名對照見該檔頂端註解。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { createInputBatcher, dataToOps, type ShellOp } from './xtermInput.ts'

const T = (text: string): ShellOp => ({ kind: 'text', text })
const K = (...keys: string[]): ShellOp => ({ kind: 'keys', keys })

test('可列印字元與空字串', () => {
  assert.deepEqual(dataToOps('abc'), [T('abc')])
  assert.deepEqual(dataToOps(' '), [T(' ')])
  assert.deepEqual(dataToOps(''), [])
})

test('多位元組 UTF-8 與 emoji 整個當 text，不被切開', () => {
  assert.deepEqual(dataToOps('中文😀'), [T('中文😀')])
  assert.deepEqual(dataToOps('é'), [T('é')])
})

test('Enter、Backspace、Tab、Shift-Tab、Esc', () => {
  assert.deepEqual(dataToOps('\r'), [K('enter')])
  assert.deepEqual(dataToOps('\x7f'), [K('backspace')])
  assert.deepEqual(dataToOps('\b'), [K('backspace')])
  assert.deepEqual(dataToOps('\t'), [K('tab')])
  assert.deepEqual(dataToOps('\x1b[Z'), [K('shift+tab')])
  assert.deepEqual(dataToOps('\x1b'), [K('esc')])
})

test('Ctrl-A..Z 對應 ctrl+a..ctrl+z；Tab、Enter、Backspace 不算 Ctrl 字母', () => {
  assert.deepEqual(dataToOps('\x01'), [K('ctrl+a')])
  assert.deepEqual(dataToOps('\x03'), [K('ctrl+c')])
  assert.deepEqual(dataToOps('\x12'), [K('ctrl+r')])
  assert.deepEqual(dataToOps('\x1a'), [K('ctrl+z')])
  // \x08 = Backspace、\x09 = Tab、\x0d = Enter，不是 ctrl+h／ctrl+i／ctrl+m。
  assert.deepEqual(dataToOps('\x08\x09\x0d'), [K('backspace', 'tab', 'enter')])
})

test('方向鍵：CSI 與 SS3 兩種形式都認', () => {
  assert.deepEqual(dataToOps('\x1b[A'), [K('up')])
  assert.deepEqual(dataToOps('\x1bOA'), [K('up')])
  assert.deepEqual(dataToOps('\x1b[B'), [K('down')])
  assert.deepEqual(dataToOps('\x1bOB'), [K('down')])
  assert.deepEqual(dataToOps('\x1b[C'), [K('right')])
  assert.deepEqual(dataToOps('\x1bOC'), [K('right')])
  assert.deepEqual(dataToOps('\x1b[D'), [K('left')])
  assert.deepEqual(dataToOps('\x1bOD'), [K('left')])
})

test('含修飾鍵的方向鍵與功能鍵：ctrl／alt／shift 前綴順序照 herdrKeyFromEvent', () => {
  assert.deepEqual(dataToOps('\x1b[1;5A'), [K('ctrl+up')])
  assert.deepEqual(dataToOps('\x1b[1;2D'), [K('shift+left')])
  assert.deepEqual(dataToOps('\x1b[1;3B'), [K('alt+down')])
  assert.deepEqual(dataToOps('\x1b[1;6C'), [K('ctrl+shift+right')])
  assert.deepEqual(dataToOps('\x1b[1;7A'), [K('ctrl+alt+up')])
  assert.deepEqual(dataToOps('\x1b[1;5P'), [K('ctrl+f1')])
  assert.deepEqual(dataToOps('\x1b[15;5~'), [K('ctrl+f5')])
})

test('功能鍵 F1–F12：SS3 與 CSI ~ 兩種形式', () => {
  assert.deepEqual(dataToOps('\x1bOP'), [K('f1')])
  assert.deepEqual(dataToOps('\x1bOS'), [K('f4')])
  assert.deepEqual(dataToOps('\x1b[11~'), [K('f1')])
  assert.deepEqual(dataToOps('\x1b[15~'), [K('f5')])
  assert.deepEqual(dataToOps('\x1b[17~'), [K('f6')])
  assert.deepEqual(dataToOps('\x1b[21~'), [K('f10')])
  assert.deepEqual(dataToOps('\x1b[23~'), [K('f11')])
  assert.deepEqual(dataToOps('\x1b[24~'), [K('f12')])
  const all = Array.from({ length: 12 }, (_, n) => `f${n + 1}`)
  const seqs = ['\x1bOP', '\x1bOQ', '\x1bOR', '\x1bOS', '\x1b[15~', '\x1b[17~', '\x1b[18~', '\x1b[19~', '\x1b[20~', '\x1b[21~', '\x1b[23~', '\x1b[24~']
  assert.deepEqual(dataToOps(seqs.join('')), [K(...all)])
})

test('Home／End／Insert／Delete／PgUp／PgDn 與 F13 以上：herdr 無可驗證鍵名，原樣當 text 送', () => {
  assert.deepEqual(dataToOps('\x1b[H'), [T('\x1b[H')])
  assert.deepEqual(dataToOps('\x1b[F'), [T('\x1b[F')])
  assert.deepEqual(dataToOps('\x1bOH'), [T('\x1bOH')])
  assert.deepEqual(dataToOps('\x1b[1~'), [T('\x1b[1~')])
  assert.deepEqual(dataToOps('\x1b[2~'), [T('\x1b[2~')])
  assert.deepEqual(dataToOps('\x1b[3~'), [T('\x1b[3~')])
  assert.deepEqual(dataToOps('\x1b[4~'), [T('\x1b[4~')])
  assert.deepEqual(dataToOps('\x1b[5~'), [T('\x1b[5~')])
  assert.deepEqual(dataToOps('\x1b[6~'), [T('\x1b[6~')])
  assert.deepEqual(dataToOps('\x1b[25~'), [T('\x1b[25~')])
  // 認不得的 CSI 也一樣退回 text，不吞掉後面的字元（相鄰 text 合併）。
  assert.deepEqual(dataToOps('\x1b[99qz'), [T('\x1b[99qz')])
})

test('Alt + 字元：alt+x；Alt+Enter、Alt+Backspace、Alt+Space', () => {
  assert.deepEqual(dataToOps('\x1bx'), [K('alt+x')])
  assert.deepEqual(dataToOps('\x1bX'), [K('alt+X')])
  assert.deepEqual(dataToOps('\x1b.'), [K('alt+.')])
  assert.deepEqual(dataToOps('\x1b\r'), [K('alt+enter')])
  assert.deepEqual(dataToOps('\x1b\x7f'), [K('alt+backspace')])
  assert.deepEqual(dataToOps('\x1b '), [K('alt+space')])
  assert.deepEqual(dataToOps('\x1b\x01'), [K('alt+ctrl+a')])
  assert.deepEqual(dataToOps('\x1b\x1b'), [K('esc', 'esc')])
})

test('ESC 在字串尾端是 Esc 鍵；未完成的 CSI 退回 text', () => {
  assert.deepEqual(dataToOps('a\x1b'), [T('a'), K('esc')])
  assert.deepEqual(dataToOps('\x1b['), [T('\x1b[')])
})

test('Ctrl-\\、Ctrl-]、Ctrl-@ 等沒有可驗證鍵名的控制字元：原樣當 text', () => {
  assert.deepEqual(dataToOps('\x1c'), [T('\x1c')])
  assert.deepEqual(dataToOps('\x1d'), [T('\x1d')])
  assert.deepEqual(dataToOps('\x00'), [T('\x00')])
})

test('貼上：bracketed paste 整段當 text，外層標記不送', () => {
  assert.deepEqual(dataToOps('\x1b[200~hello world\x1b[201~'), [T('hello world')])
  assert.deepEqual(dataToOps('\x1b[200~\x1b[201~'), [])
  // 貼上的 \r 轉 Enter 鍵，\n 留在 text 裡；\r\n 只算一次 Enter。
  assert.deepEqual(dataToOps('\x1b[200~hello\rworld\x1b[201~'), [T('hello'), K('enter'), T('world')])
  assert.deepEqual(dataToOps('\x1b[200~a\r\nb\nc\x1b[201~'), [T('a'), K('enter'), T('b\nc')])
  // 貼上前後的一般字元、控制字元照常轉換並與 text 合併。
  assert.deepEqual(dataToOps('x\x1b[200~y\x1b[201~z'), [T('xyz')])
  assert.deepEqual(dataToOps('\x1b[200~ls\x1b[201~\r'), [T('ls'), K('enter')])
})

test('貼上內容沒有結尾標記：貼到字串尾端', () => {
  assert.deepEqual(dataToOps('\x1b[200~abc'), [T('abc')])
})

test('相鄰 text 合併、相鄰 keys 合併，順序不亂', () => {
  assert.deepEqual(dataToOps('\r\r'), [K('enter', 'enter')])
  assert.deepEqual(dataToOps('ab\x1b[Ac'), [T('ab'), K('up'), T('c')])
  assert.deepEqual(dataToOps('a\x03b'), [T('a'), K('ctrl+c'), T('b')])
  assert.deepEqual(dataToOps('\x1b[A\x1b[B\x1b[C\x1b[D'), [K('up', 'down', 'right', 'left')])
  assert.deepEqual(dataToOps('ls -la\r'), [T('ls -la'), K('enter')])
  assert.deepEqual(dataToOps('\x1b[Z\t'), [K('shift+tab', 'tab')])
})

test('createInputBatcher：同一 frame 內多次 onData 合併成一次 send，順序保持', () => {
  const sent: ShellOp[][] = []
  let pending: (() => void) | null = null
  let schedules = 0
  const batch = createInputBatcher(
    (ops) => sent.push(ops),
    (cb) => {
      schedules++
      pending = cb
    },
  )
  batch('l')
  batch('s')
  batch('\x1b[A')
  batch('\r')
  assert.equal(schedules, 1, '同一 frame 只排一次')
  assert.deepEqual(sent, [], '排程前不送')
  pending!()
  assert.deepEqual(sent, [[T('ls'), K('up', 'enter')]])
})

test('createInputBatcher：空字串不送、不排程；flush 後的下一個 frame 重新排程', () => {
  const sent: ShellOp[][] = []
  const queue: Array<() => void> = []
  const batch = createInputBatcher(
    (ops) => sent.push(ops),
    (cb) => queue.push(cb),
  )
  batch('')
  assert.equal(queue.length, 0)
  batch('a')
  queue.shift()!()
  batch('b')
  assert.equal(queue.length, 1)
  queue.shift()!()
  assert.deepEqual(sent, [[T('a')], [T('b')]])
})

test('createInputBatcher：flush 時沒有可送的 op（例如只有貼上空字串）就不 send', () => {
  const sent: ShellOp[][] = []
  const queue: Array<() => void> = []
  const batch = createInputBatcher(
    (ops) => sent.push(ops),
    (cb) => queue.push(cb),
  )
  batch('\x1b[200~\x1b[201~')
  queue.shift()!()
  assert.deepEqual(sent, [])
})

test('createInputBatcher：一個序列被切在兩次 onData 之間，仍解析成同一個鍵', () => {
  const sent: ShellOp[][] = []
  const queue: Array<() => void> = []
  const batch = createInputBatcher(
    (ops) => sent.push(ops),
    (cb) => queue.push(cb),
  )
  batch('\x1b[')
  batch('A')
  queue.shift()!()
  assert.deepEqual(sent, [[K('up')]])
})
