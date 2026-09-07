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

test('輸出是在比 columns 窄的時候印的：拿最長的一行當折行寬度', () => {
  const l1 = "If the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true&client_id"
  const l2 = '=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.'
  const l3 = 'd4WT3wVFX-JmdZbLPqMA'
  const l4 = 'Paste code here if prompted >'
  const rows = termPieces([l1, l2, l3, l4].join('\n'), 185)
  const full = l1.slice(35) + l2 + l3
  assert.deepEqual(urlsOf(rows), [[l1.slice(35), full], [l2, full], [l3, full]])
  assert.equal(rows[3][0].text, l4)
})
