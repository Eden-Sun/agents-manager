import test from 'node:test'
import assert from 'node:assert/strict'
import { linksOnRow, type CellRow } from './xtermLinks'

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
