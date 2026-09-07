import assert from 'node:assert/strict'
import { test } from 'node:test'
import { termPieces } from './TermLinks'

const urlsOf = (rows: ReturnType<typeof termPieces>) => rows.flat().filter((p) => p.url).map((p) => [p.text, p.url])

test('被終端折行的 URL：每一段都指向接回來的完整 URL', () => {
  const cols = 40
  const l1 = 'visit: https://a.example/x?'.padEnd(cols, 'a')
  const l2 = 'bbb&c=1 rest'
  const full = l1.slice(7) + 'bbb&c=1'
  assert.deepEqual(urlsOf(termPieces(`${l1}\n${l2}`, cols)), [
    [l1.slice(7), full],
    ['bbb&c=1', full],
  ])
})

test('沒塞滿寬度就不接，下一行的提示字元不會被吃掉', () => {
  const rows = termPieces('see https://a.example/x\nm4p@m4p %', 80)
  assert.deepEqual(urlsOf(rows), [['https://a.example/x', 'https://a.example/x']])
  assert.equal(rows[1][0].text, 'm4p@m4p %')
})

test('連續折兩行，最後一段沒塞滿就停', () => {
  const cols = 20
  const l1 = 'https://a.example/'.padEnd(cols, 'p')
  const l2 = 'q'.repeat(cols)
  const l3 = 'z end'
  const rows = termPieces([l1, l2, l3].join('\n'), cols)
  const full = l1 + l2 + 'z'
  assert.deepEqual(urlsOf(rows), [[l1, full], [l2, full], ['z', full]])
  assert.equal(rows[2][1].text, ' end')
})

test('句尾標點不算', () => {
  assert.deepEqual(urlsOf(termPieces('go https://a.example/x.', 80)), [['https://a.example/x.', 'https://a.example/x']])
})
