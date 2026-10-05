import assert from 'node:assert/strict'
import { test } from 'node:test'
import { termPieces } from '../lib/termPieces.ts'

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

/** 2026-09-08 實際壞掉的畫面：OAuth URL 在 93 欄折 5 行，但快照有 600+ 欄的行，拿最長行當寬度就只連到第一段。 */
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

test('已知 columns：最長但沒滿欄的 URL 不把下一行提示字元接進去', () => {
  const rows = termPieces(
    'm4p@m4p % git push\nremote: Create a pull request: https://github.com/o/r/pull/new/feature-x\nm4p@m4p % ',
    120,
  )
  assert.deepEqual(urlsOf(rows), [['https://github.com/o/r/pull/new/feature-x', 'https://github.com/o/r/pull/new/feature-x']])
  assert.equal(rows[2][0].url, null)
  assert.match(rows[2][0].text, /^m4p@m4p/)
})

test('括號包住的 URL 不把不成對的結尾括號算進去', () => {
  assert.equal(urlsOf(termPieces('see (https://example.com/docs/page) here', 80))[0][1], 'https://example.com/docs/page')
  assert.equal(urlsOf(termPieces('[docs](https://example.com/docs/page)', 80))[0][1], 'https://example.com/docs/page')
  // 網址自己的括號是成對的，要留著。
  assert.equal(
    urlsOf(termPieces('https://en.wikipedia.org/wiki/Foo_(bar)', 80))[0][1],
    'https://en.wikipedia.org/wiki/Foo_(bar)',
  )
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

/** 2026-10-05 實測：agy 登入的 OAuth 網址（213 欄 pane 折在 212），續列開頭是 agy 的 1 格邊界。`herdr pane read --source visible` 原樣。 */
const AGY_SCREEN = [
  " Your browser should open automatically. If not:",
  "",
  " https://accounts.google.com/o/oauth2/auth?access_type=offline&client_id=1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com&code_challenge=j5VIs5EZlVJvtpQhu5QRYv3IRX_et4K4g7Giaav97gY&code_c",
  " hallenge_method=S256&prompt=consent&redirect_uri=https%3A%2F%2Fantigravity.google%2Foauth-callback&response_type=code&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform+https%3A%2F%2Fwww.googleapis.c",
  " om%2Fauth%2Fuserinfo.email+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.profile+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcclog+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fexperimentsandconfigs+https%3A%2F%2",
  " Fwww.googleapis.com%2Fauth%2Faicode+openid&state=nyhyXCUTff-RW3SUqTgAMg",
  "",
  " Copy and paste the URL or click on the link below:"
]
const AGY_URL = AGY_SCREEN.slice(2, 6).map((l) => l.slice(1)).join('')
const joined = (row: ReturnType<typeof termPieces>[number]) => row.map((p) => p.text).join('')

test('agy 登入的長網址：續列開頭多一格空白也接回，每一段都指向完整網址', () => {
  const rows = termPieces(AGY_SCREEN.join('\n'), 213)
  const urls = rows.flat().filter((p) => p.url)
  assert.equal(urls.length, 4, '四列各一段')
  for (const p of urls) assert.equal(p.url, AGY_URL)
  assert.match(AGY_URL, /^https:\/\/accounts\.google\.com\/o\/oauth2\/auth\?.*code_challenge=.*&state=nyhyXCUTff-RW3SUqTgAMg$/)
  assert.ok(AGY_URL.includes('code_challenge_method=S256'), '第一個接縫（code_c｜hallenge）')
  assert.ok(AGY_URL.includes('www.googleapis.com%2Fauth%2Fuserinfo.email'), '第二個接縫（googleapis.c｜om）')
  assert.ok(AGY_URL.includes('%3A%2F%2Fwww.googleapis.com%2Fauth%2Faicode'), '第三個接縫（%2F%2｜Fwww）')
  // 版面不動：續列的開頭空白仍是純文字，前後文也沒被吃掉。
  assert.equal(joined(rows[0]), AGY_SCREEN[0])
  assert.equal(joined(rows[3]), AGY_SCREEN[3])
  assert.equal(joined(rows[7]), AGY_SCREEN[7])
  assert.equal(rows[7].some((p) => p.url), false, '「Copy and paste…」不是網址')
})

test('agy 的網址：欄寬不明也接回（續片同寬、最長行當證據）', () => {
  const urls = termPieces(AGY_SCREEN.join('\n')).flat().filter((p) => p.url)
  assert.equal(urls.length, 4)
  assert.equal(urls[0].url, AGY_URL)
})

test('縮一格的續列：前一列沒塞滿寬度就不接（一般縮排文字不被誤接）', () => {
  const rows = termPieces(['  see https://a.example/path', ' Done', ' Copy and paste the URL'].join('\n'), 80)
  assert.deepEqual(urlsOf(rows), [['https://a.example/path', 'https://a.example/path']])
  assert.equal(joined(rows[1]), ' Done')
})

test('縮一格的續列：有空白的整句不接，就算前一列塞滿了', () => {
  const cols = 30
  const l1 = ' https://a.example/'.padEnd(cols - 1, 'p')
  const rows = termPieces([l1, ' Copy and paste the URL'].join('\n'), cols)
  assert.deepEqual(urlsOf(rows), [[l1.slice(1), l1.slice(1)]])
})

test('縮一格的續列：續片要從網址字元開頭（空白後接右括號不接）', () => {
  const cols = 30
  const l1 = ' https://a.example/'.padEnd(cols - 1, 'p')
  assert.deepEqual(urlsOf(termPieces([l1, ' ）oops'].join('\n'), cols)), [[l1.slice(1), l1.slice(1)]])
})

test('縮一格的續列：最後一列短，網址在那裡結束，後面的提示字不受影響', () => {
  const cols = 30
  const l1 = ' https://a.example/'.padEnd(cols - 1, 'p')
  const l2 = ' ' + 'q'.repeat(cols - 2)
  const rows = termPieces([l1, l2, ' tail', '', ' $ prompt'].join('\n'), cols)
  const full = l1.slice(1) + l2.slice(1) + 'tail'
  assert.deepEqual(urlsOf(rows), [[l1.slice(1), full], [l2.slice(1), full], ['tail', full]])
  assert.equal(joined(rows[4]), ' $ prompt')
})

test('不縮排的折行照舊（不被縮一格的規則改壞）', () => {
  const cols = 20
  const l1 = 'https://a.example/'.padEnd(cols, 'p')
  assert.equal(urlsOf(termPieces([l1, 'q'.repeat(cols), 'z end'].join('\n'), cols)).length, 3)
})
