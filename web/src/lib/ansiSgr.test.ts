import assert from 'node:assert/strict'
import { test } from 'node:test'
import { parseAnsi, stripAnsi } from './ansiSgr.ts'

const E = '\x1b'

test('純文字：一段、空樣式', () => {
  assert.deepEqual(parseAnsi('hello\nworld'), [{ text: 'hello\nworld', style: {} }])
  assert.deepEqual(parseAnsi(''), [])
})

test('16 色前景／背景與亮色', () => {
  const segs = parseAnsi(`${E}[31mred${E}[0m ${E}[42mg${E}[0m ${E}[92mbright${E}[m`)
  assert.deepEqual(segs, [
    { text: 'red', style: { fg: 'ansi:1' } },
    { text: ' ', style: {} },
    { text: 'g', style: { bg: 'ansi:2' } },
    { text: ' ', style: {} },
    { text: 'bright', style: { fg: 'ansi:10' } },
  ])
})

test('粗體、淡、斜、底線、反白、刪除線與各自的取消', () => {
  const segs = parseAnsi(`${E}[1;2;3;4;7;9mA${E}[22mB${E}[23;24;27;29mC`)
  assert.deepEqual(segs[0].style, { bold: true, dim: true, italic: true, underline: true, inverse: true, strike: true })
  assert.deepEqual(segs[1].style, { italic: true, underline: true, inverse: true, strike: true })
  assert.deepEqual(segs[2].style, {})
})

test('39／49 只清前景、背景；0 全清', () => {
  const segs = parseAnsi(`${E}[31;42;1mA${E}[39mB${E}[49mC${E}[0mD`)
  assert.deepEqual(segs.map((s) => s.style), [
    { fg: 'ansi:1', bg: 'ansi:2', bold: true },
    { bg: 'ansi:2', bold: true },
    { bold: true },
    {},
  ])
})

test('256 色：16 以下是主題變數，16 以上換成固定色；灰階', () => {
  const segs = parseAnsi(`${E}[38;5;9mA${E}[48;5;196mB${E}[38;5;16mC${E}[38;5;244mD`)
  assert.equal(segs[0].style.fg, 'ansi:9')
  assert.equal(segs[1].style.bg, '#ff0000')
  assert.equal(segs[2].style.fg, '#000000')
  assert.equal(segs[3].style.fg, '#808080')
})

test('truecolor 38;2;r;g;b 與 48;2；超出範圍的分量夾到 0–255', () => {
  const segs = parseAnsi(`${E}[38;2;255;128;0mA${E}[48;2;300;0;16mB`)
  assert.equal(segs[0].style.fg, '#ff8000')
  assert.equal(segs[1].style.bg, '#ff0010')
})

test('冒號子參數：38:5:n 與 38:2::r:g:b', () => {
  const segs = parseAnsi(`${E}[38:5:2mA${E}[38:2::1:2:3mB`)
  assert.equal(segs[0].style.fg, 'ansi:2')
  assert.equal(segs[1].style.fg, '#010203')
})

test('同樣樣式的相鄰字元合併成一段；SGR 參數串可以混多個序列', () => {
  const segs = parseAnsi(`a${E}[1mb${E}[1mc`)
  assert.deepEqual(segs, [
    { text: 'a', style: {} },
    { text: 'bc', style: { bold: true } },
  ])
})

test('游標移動、清除行、其他 CSI 整段丟掉，不留下參數字元', () => {
  const raw = `x${E}[2K${E}[1;5H${E}[?25l${E}[3Ay${E}[s${E}[uz`
  assert.equal(stripAnsi(raw), 'xyz')
  assert.equal(parseAnsi(raw).every((s) => Object.keys(s.style).length === 0), true)
})

test('帶中間位元組的 CSI（例如 ESC [ 1 SP q）不當成 SGR', () => {
  assert.equal(stripAnsi(`a${E}[1 qb`), 'ab')
  assert.deepEqual(parseAnsi(`a${E}[1 qb`)[0].style, {})
})

test('OSC 非 8 的序列丟掉（BEL 與 ST 結尾都行）；標題設定不外洩', () => {
  assert.equal(stripAnsi(`a${E}]0;title\x07b${E}]2;t${E}\\c`), 'abc')
})

test('OSC 8 超連結：顯示文字保留並帶 href，結束後回到無連結', () => {
  const raw = `see ${E}]8;;https://example.com/x${E}\\the link${E}]8;;${E}\\ end`
  const segs = parseAnsi(raw)
  assert.deepEqual(segs, [
    { text: 'see ', style: {} },
    { text: 'the link', style: {}, href: 'https://example.com/x' },
    { text: ' end', style: {} },
  ])
  assert.equal(stripAnsi(raw), 'see the link end')
})

test('OSC 8 與 SGR 可以疊：連結文字帶顏色', () => {
  const segs = parseAnsi(`${E}[34m${E}]8;;https://a.example${E}\\go${E}]8;;${E}\\${E}[0m`)
  assert.deepEqual(segs, [{ text: 'go', style: { fg: 'ansi:4' }, href: 'https://a.example' }])
})

test('\\r 與其他控制字元丟掉；\\t 與 \\n 保留', () => {
  assert.equal(stripAnsi('a\rb\x08c\x07d\te\nf\x7fg'), 'abcd\te\nfg')
  assert.equal(stripAnsi('a\r\nb'), 'a\nb')
})

test('未結束的跳脫序列在結尾被吃掉，不會留下 ESC', () => {
  assert.equal(stripAnsi(`ok${E}[31`), 'ok')
  assert.equal(stripAnsi(`ok${E}]8;;https://x`), 'ok')
  assert.equal(stripAnsi(`ok${E}`), 'ok')
  assert.ok(!stripAnsi(`${E}P1$r;\x9b31m`).includes(E))
})

test('stripAnsi 等於片段串接；輸出沒有原始 ESC 或 C0 控制字元（除 \\n \\t）', () => {
  const raw = `${E}[1;31mERR${E}[0m\r\n${E}[38;5;200m${E}[2K${E}]8;;https://a.example/b\x07link${E}]8;;\x07\t${E}[?1049h`
  const joined = parseAnsi(raw).map((s) => s.text).join('')
  assert.equal(stripAnsi(raw), joined)
  const bad = [...joined].filter((c) => c !== '\n' && c !== '\t' && (c.charCodeAt(0) < 0x20 || c.charCodeAt(0) === 0x7f))
  assert.deepEqual(bad, [])
})
