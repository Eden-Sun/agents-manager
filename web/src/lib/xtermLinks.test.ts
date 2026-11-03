import test from 'node:test'
import assert from 'node:assert/strict'
import { linksOnRow, wrapWidth, type CellRow } from './xtermLinks'

const cells = (s: string, cols: number): CellRow => [...s.padEnd(cols, ' ')].slice(0, cols)

test('一列內的網址：起訖格子', () => {
  const rows = [cells('  https://auth.openai.com/codex/device', 50)]
  const [l] = linksOnRow((y) => rows[y], 0)
  assert.equal(l.url, 'https://auth.openai.com/codex/device')
  assert.deepEqual(l.start, { x: 2, y: 0 })
  assert.deepEqual(l.end, { x: 37, y: 0 })
})

test('被折到下一列的長網址：從任一列都拿得到整串', () => {
  const url = 'https://claude.com/cai/oauth/authorize?code=true&client_id=abc'
  const line = `go: ${url}`
  const rows: CellRow[] = []
  for (let i = 0; i < line.length; i += 20) rows.push(cells(line.slice(i, i + 20), 20))
  const last = rows.length - 1
  rows.push(cells('next line', 20))
  for (let y = 0; y <= last; y++) {
    const [l] = linksOnRow((i) => rows[i], y)
    assert.equal(l.url, url, `row ${y}`)
    assert.deepEqual(l.start, { x: 4, y: 0 })
    assert.equal(l.end.y, last)
  }
  assert.deepEqual(linksOnRow((i) => rows[i], last + 1), [])
})

test('沒寫滿的列不跟下一列接；結尾標點不算網址', () => {
  const rows = [cells('see https://a.example/x.', 40), cells('more', 40)]
  const [l] = linksOnRow((y) => rows[y], 0)
  assert.equal(l.url, 'https://a.example/x')
  assert.deepEqual(linksOnRow((y) => rows[y], 1), [])
})

test('寬字佔兩格：網址的欄位照格子算', () => {
  const row: CellRow = ['登', '', '入', '', ' ', ...'https://x.example/a'.split(''), ...Array(10).fill(' ')]
  const [l] = linksOnRow(() => row, 0)
  assert.deepEqual(l.start, { x: 5, y: 0 })
  assert.equal(l.url, 'https://x.example/a')
})

test('pane 比 xterm 欄數窄：折行的列最後幾格是空的，仍從任一列拿到整串', () => {
  // 實測：xterm 213 欄、pane 實際 212 欄；204 字網址 + 長 a 串折成 212 + 餘。
  const cols = 213
  const url = `https://example.com/${'a'.repeat(300)}?x=1`
  const rows: CellRow[] = [cells(url.slice(0, 212), cols), cells(url.slice(212), cols), cells('ubuntu@ubuntu:~$ ', cols)]
  const wrapAt = wrapWidth(rows, cols)
  assert.equal(wrapAt, 212)
  for (const y of [0, 1]) {
    const [l] = linksOnRow((i) => rows[i], y, wrapAt)
    assert.equal(l.url, url, `row ${y}`)
    assert.equal(l.url.length, 324)
    assert.deepEqual(l.end, { x: url.length - 212 - 1, y: 1 })
  }
  assert.deepEqual(linksOnRow((i) => rows[i], 2, wrapAt), [])
})

test('畫面上沒有折行（最寬的列離右邊很遠）：行尾的網址不跟下一列接', () => {
  const cols = 213
  const rows: CellRow[] = [cells('see https://a.example/abc', cols), cells('ubuntu@ubuntu:~$ ', cols)]
  assert.equal(wrapWidth(rows, cols), null)
  const [l] = linksOnRow((i) => rows[i], 0, wrapWidth(rows, cols))
  assert.equal(l.url, 'https://a.example/abc')
})
