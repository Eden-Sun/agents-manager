import test from 'node:test'
import assert from 'node:assert/strict'
import { findHardcodedCjk, isUiSource, scanKeys } from './i18nScan'

const head = "import { t, msg } from '../lib/i18n'\n"

test('抽 key：t() 與 msg() 的字面字串與插值名稱', () => {
  const { uses, problems } = scanKeys('a.tsx', head + "const a = t('你好 {name}', { name })\nconst b = msg('未安裝')\nconst c = t(`多行`)\n")
  assert.deepEqual(uses.map((u) => [u.key, u.vars]), [['你好 {name}', ['name']], ['未安裝', []], ['多行', []]])
  assert.deepEqual(problems, [])
})

test('t.x 塞元件的句子也抽 key，tr 不檢查', () => {
  const { uses, problems } = scanKeys('a.tsx', head + "t.x('改用 {cmd} 開發', { cmd: <code>x</code> })\nt.tr(k)\nt.x(k, {})\n")
  assert.deepEqual(uses.map((u) => [u.key, u.vars]), [['改用 {cmd} 開發', ['cmd']]])
  assert.equal(problems.length, 1)
  assert.match(problems[0].message, /t\.x\(\)/)
})

test('抓出：動態 key、漏傳插值、多傳插值', () => {
  const { problems } = scanKeys('a.ts', head + "t(k)\nt('{a} {b}', { a: 1 })\nt('{a}', { a: 1, z: 2 })\nt(`x${y}`)\nt('{a}', { ...rest })\n")
  assert.equal(problems.length, 4)
  assert.match(problems[0].message, /字面字串/)
  assert.match(problems[1].message, /缺插值 \{b\}/)
  assert.match(problems[2].message, /多傳了 z/)
  assert.match(problems[3].message, /字面字串/)
})

test('沒 import i18n 的檔案不掃（別的 t 函式不算）', () => {
  assert.deepEqual(scanKeys('a.ts', "const t = (x) => x\nt(foo)\n").uses, [])
})

test('寫死中文：JSX 文字、字串、樣板字串都抓；t()／import／註解／i18n-ignore 不算', () => {
  const src = [
    "import { t } from './i18n'",
    "import x from './中文路徑'",
    'export const A = () => (',
    '  <div title="標題">',
    '    哈囉',
    "    {t('已翻譯')}",
    '    {`第${n}個`}',
    '    {/* 註解中文 */}',
    "    {'繁體中文' /* i18n-ignore */}",
    '  </div>',
    ')',
    "const ok = 'English' // i18n-ignore 中文",
    "const bad = '沒抽'",
  ].join('\n')
  const hits = findHardcodedCjk('a.tsx', src).map((h) => h.text)
  assert.deepEqual(hits, ['標題', '哈囉', '第', '個', '沒抽'])
})

test('哪些檔案要翻：略過測試、字典、i18n 工具、mock 假資料與 testing', () => {
  assert.equal(isUiSource('src/components/A.tsx'), true)
  assert.equal(isUiSource('src/components/A.test.tsx'), false)
  assert.equal(isUiSource('src/i18n/en/chat.ts'), false)
  assert.equal(isUiSource('src/lib/i18n/index.ts'), false)
  assert.equal(isUiSource('src/api/mock.ts'), false)
  assert.equal(isUiSource('src/api/mockShare.ts'), false)
  assert.equal(isUiSource('src/api/types.ts'), true)
  assert.equal(isUiSource('src/testing/sharedMock.ts'), false)
  assert.equal(isUiSource('src/styles.css'), false)
})
