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

/**
 * 實際壞掉的畫面（2026-09-08，bot `o2-low` 的終端分頁）。claude 登入畫面的 OAuth URL 在 93 欄
 * 折成 5 行，但同一份快照裡有兩條 600 欄以上的行——`recent_unwrapped` 已經幫 shell 指令接回原長。
 *
 * 舊邏輯拿「畫面上最長的一行」當折行寬度（618），URL 那五行都判定成沒塞滿，於是只有第一行變成
 * 連結，點到／複製到的是腰斬的 `…88ed-5944d1962f`，登入必失敗。
 */
const OAUTH_URL =
  'https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e' +
  '&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback' +
  '&scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference+user%3Asessions%3Aclaude_code' +
  '+user%3Amcp_servers+user%3Afile_upload&code_challenge=aRZiMv8rovhGSTmvi9q5X-0DUq2VwgL4qzFvi0YimMQ' +
  '&code_challenge_method=S256&state=rxA0wQpSnYlV3xFmeJFy0Dvay-kMij1_CzrOqBYUi38'

/** 把 URL 依終端寬度切成一段段，模擬軟折行。 */
const wrapAt = (s: string, w: number) => {
  const out: string[] = []
  for (let i = 0; i < s.length; i += w) out.push(s.slice(i, i + w))
  return out
}

test('登入畫面：URL 在 93 欄折成 5 行，同一份快照裡另有 618 欄的長行', () => {
  const chunks = wrapAt(OAUTH_URL, 93)
  assert.deepEqual(chunks.map((c) => c.length), [93, 93, 93, 93, 78], '這條測資本來就要折成 5 行')
  const lines = [
    // `recent_unwrapped` 把 shell 指令接回原長，比 URL 的折行寬度長得多。
    `m4p@m4p rt % claude --dangerously-skip-permissions --settings ${'x'.repeat(556)}`,
    '',
    " Browser didn't open? Use the url below to sign in (c to copy)",
    '',
    ...chunks,
    '',
    ' Paste code here if prompted >',
  ]
  assert.equal(Math.max(...lines.map((l) => l.length)), 618)
  const rows = termPieces(lines.join('\n'), 185)
  const links = urlsOf(rows)
  // 五段都在，每一段都指向同一條完整 URL；接起來等於原字串（複製出去就是這個）
  assert.equal(links.length, 5)
  assert.deepEqual(links.map(([text]) => text), chunks)
  for (const [, url] of links) assert.equal(url, OAUTH_URL)
  assert.equal(links.map(([text]) => text).join(''), OAUTH_URL)
  // 尾巴沒有被吃進 URL
  assert.equal(rows[rows.length - 1][0].text, ' Paste code here if prompted >')
})

test('URL 剛好在行尾結束、下一行是新的一行：不接（不是每個行尾 URL 都是折行）', () => {
  // 這行 60 字（≥ MIN_WRAP_WIDTH）但既不等於 columns、也不是最長的一行，
  // 下一行也不是同寬的續行 → 不能黏。
  const l1 = 'open https://a.example/' + 'q'.repeat(37)
  assert.equal(l1.length, 60)
  const rows = termPieces(['x'.repeat(185), l1, 'm4p@m4p %'].join('\n'), 185)
  assert.deepEqual(urlsOf(rows), [[l1.slice(5), l1.slice(5)]])
  assert.equal(rows[2][0].text, 'm4p@m4p %')
})
