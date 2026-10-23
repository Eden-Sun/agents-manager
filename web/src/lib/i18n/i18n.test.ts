import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { setupDom, teardownDom } from '../../testing/domHarness'
import { detectSystemLang, getLang, getLangPref, langFromSearch, langFromStorageEvent, loadLangPref, resetLangForTest, setLangOverride, setLangPref, subscribeLang } from './lang'
import { interpolate, mergeDicts, placeholders, translate, translateParts } from './translate'
import { fmtNumber, msg, t, tr } from './index'

before(setupDom)
after(teardownDom)
afterEach(() => {
  localStorage.clear()
  resetLangForTest()
})

const dict = {
  '已連線': 'Connected',
  '{n} 個 Bot': { one: '{n} Bot', other: '{n} Bots' },
  '你好 {name}': 'Hello {name}',
}

test('插值：{name} 換成值、沒給的原樣留著、不是識別字的大括號不動', () => {
  assert.equal(interpolate('你好 {name}，{n} 個', { name: 'A', n: 3 }), '你好 A，3 個')
  assert.equal(interpolate('你好 {name}', {}), '你好 {name}')
  assert.equal(interpolate('JSON {} 與 { a } 與 {1}', { a: 'x' }), 'JSON {} 與 { a } 與 {1}')
  assert.deepEqual(placeholders('{a} 和 {b} 和 {a}'), ['a', 'b'])
})

test('翻譯：繁中用 key 本身、英文查字典、缺了退回繁中並記下', () => {
  const missing: string[] = []
  assert.equal(translate('zh-TW', dict, '你好 {name}', { name: 'A' }), '你好 A')
  assert.equal(translate('en', dict, '你好 {name}', { name: 'A' }), 'Hello A')
  assert.equal(translate('en', dict, '沒翻譯 {x}', { x: 1 }, (k) => missing.push(k)), '沒翻譯 1')
  assert.deepEqual(missing, ['沒翻譯 {x}'])
  translate('en', dict, 'Claude API', undefined, (k) => missing.push(k))
  assert.equal(missing.length, 1, '沒有中文的句子不算缺翻譯')
  assert.equal(translate('en', dict, 'constructor'), 'constructor', '原型鏈上的名字不能當翻譯')
})

test('複數：n=1 用 one，其餘（含 0 與缺 n）用 other；繁中不分單複數', () => {
  assert.equal(translate('en', dict, '{n} 個 Bot', { n: 1 }), '1 Bot')
  assert.equal(translate('en', dict, '{n} 個 Bot', { n: 0 }), '0 Bots')
  assert.equal(translate('en', dict, '{n} 個 Bot', { n: 2 }), '2 Bots')
  assert.equal(translate('en', dict, '{n} 個 Bot'), '{n} Bots')
  assert.equal(translate('zh-TW', dict, '{n} 個 Bot', { n: 1 }), '1 個 Bot')
})

test('塞元件：{name} 的位置換成節點、其餘文字原樣、複數照 n 挑', () => {
  const x = <T extends string | number>(key: string, vars: Record<string, T>, lang: 'zh-TW' | 'en' = 'en') => translateParts(lang, { ...dict, '或改用 {cmd} 開發': 'Or run {cmd}', '{n} 個 {what}': { one: '{n} {what}', other: '{n} {what}s' } }, key, vars)
  assert.deepEqual(x('或改用 {cmd} 開發', { cmd: 'X' }), ['Or run ', 'X'])
  assert.deepEqual(x('或改用 {cmd} 開發', { cmd: 'X' }, 'zh-TW'), ['或改用 ', 'X', ' 開發'])
  assert.deepEqual(x('或改用 {cmd} 開發', {}), ['Or run {cmd}'], '沒給的名稱原樣留著')
  assert.deepEqual(x('{n} 個 {what}', { n: 1, what: 'Bot' }), [1, ' ', 'Bot'])
  assert.deepEqual(x('{n} 個 {what}', { n: 2, what: 'Bot' }), [2, ' ', 'Bot', 's'])
})

test('合併字典：英文相同的重複沒事、不同的列為衝突', () => {
  const { dict: merged, conflicts } = mergeDicts({ a: { x: '1', y: 'same' }, b: { y: 'same', x: '2' } })
  assert.equal(merged.y, 'same')
  assert.deepEqual(conflicts, [{ key: 'x', areas: ['a', 'b'] }])
})

test('瀏覽器語言：zh 開頭（含 zh-CN、zh-HK）→ 繁中，其餘 → 英文', () => {
  assert.equal(detectSystemLang(['zh-TW', 'en']), 'zh-TW')
  assert.equal(detectSystemLang('zh-CN'), 'zh-TW')
  assert.equal(detectSystemLang(['zh']), 'zh-TW')
  assert.equal(detectSystemLang(['en-US', 'zh-TW']), 'en', '只看第一順位')
  assert.equal(detectSystemLang('ja'), 'en')
  assert.equal(detectSystemLang(undefined), 'en')
  assert.equal(detectSystemLang('zhuang'), 'en', 'zh 要整個字，不是前綴')
})

test('網址 ?lang= 與 storage 事件', () => {
  assert.equal(langFromSearch('?lang=en'), 'en')
  assert.equal(langFromSearch('?x=1&lang=zh-TW'), 'zh-TW')
  assert.equal(langFromSearch('?lang=zh-Hant'), 'zh-TW')
  assert.equal(langFromSearch('?lang=fr'), null)
  assert.equal(langFromSearch(''), null)
  assert.equal(langFromStorageEvent('other', 'en'), false)
  assert.equal(langFromStorageEvent('am-lang', 'en'), true)
  assert.equal(getLang(), 'en')
  assert.equal(localStorage.getItem('am-lang'), null, 'storage 事件只套用、不寫回')
})

test('偏好：預設跟隨（測試環境固定繁中）、切換存起來、通知訂閱者、html lang 跟著換', () => {
  assert.equal(getLangPref(), 'system')
  assert.equal(getLang(), 'zh-TW')
  assert.equal(document.documentElement.lang, 'zh-Hant')
  let calls = 0
  const off = subscribeLang(() => calls++)
  setLangPref('en')
  assert.equal(getLang(), 'en')
  assert.equal(document.documentElement.lang, 'en')
  assert.equal(localStorage.getItem('am-lang'), 'en')
  assert.equal(calls, 1)
  setLangPref('en')
  assert.equal(calls, 1, '什麼都沒變不通知')
  setLangPref('system')
  assert.equal(localStorage.getItem('am-lang'), null, '跟隨瀏覽器＝不存')
  assert.equal(getLang(), 'zh-TW')
  assert.equal(calls, 2)
  setLangPref('zh-TW')
  assert.equal(calls, 3, '語言沒變、偏好變了也要通知（設定畫面的選取狀態靠它）')
  off()
  setLangPref('en')
  assert.equal(calls, 3, '取消訂閱後不再通知')
})

test('網址固定的語言優先於偏好、不寫入儲存；使用者在設定裡明確選了就讓位', () => {
  setLangPref('zh-TW')
  setLangOverride('en')
  assert.equal(getLang(), 'en')
  assert.equal(localStorage.getItem('am-lang'), 'zh-TW')
  setLangOverride(null)
  assert.equal(getLang(), 'zh-TW')
  setLangOverride('en')
  setLangPref('zh-TW')
  assert.equal(getLang(), 'zh-TW')
})

test('讀儲存壞掉（私密視窗、值亂寫）退回跟隨瀏覽器', () => {
  localStorage.setItem('am-lang', 'klingon')
  assert.equal(loadLangPref(), 'system')
  const real = Object.getOwnPropertyDescriptor(globalThis, 'localStorage')!
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, get: () => { throw new Error('denied') } })
  try {
    assert.equal(loadLangPref(), 'system')
    resetLangForTest()
    assert.doesNotThrow(() => setLangPref('en'))
    assert.equal(getLang(), 'en')
  } finally {
    Object.defineProperty(globalThis, 'localStorage', real)
  }
})

test('模組層級的 t／tr／msg 讀呼叫當下的語言；缺英文退回繁中；msg 原樣回傳', () => {
  assert.equal(t('已連線'), '已連線')
  setLangPref('en')
  assert.equal(t('已連線'), 'Connected')
  assert.equal(t('這句沒有英文 {n}', { n: 2 }), '這句沒有英文 2')
  assert.equal(tr(msg('未安裝')), 'Not installed')
  assert.equal(msg('未安裝'), '未安裝')
})

test('數字格式跟著語言的 locale', () => {
  assert.equal(fmtNumber(1234567.5), '1,234,567.5')
  setLangPref('en')
  assert.equal(fmtNumber(1234567.5), '1,234,567.5')
})
