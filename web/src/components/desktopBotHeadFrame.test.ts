import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const head = await readFile(new URL('./desktopBotHead.css', import.meta.url), 'utf8')

/**
 * 桌機（>1024px）標題列的名字區：#1269 之前是固定 18em，名字與 ★⚙ 之間、@m4p 與額度之間都是它吃掉的空白。
 * 檔案裡同一條規則有好幾段（後面的覆寫前面的），生效的是最後一段，所以取 lastIndexOf。
 */
const desktopMain = (() => {
  const at = head.lastIndexOf('@media (width > 1024px) {\n  .bot-head .main-title {')
  return at >= 0 ? head.slice(at, head.indexOf('\n}', at)) : ''
})()

test('桌機名字區只佔內容寬：不固定 18em、不伸縮吃多餘空間（#1269）', () => {
  assert.ok(desktopMain, 'desktopBotHead.css 要有桌機 .bot-head .main-title 規則')
  assert.match(desktopMain, /flex:\s*0 0 auto;/, '名字區不 grow 也不寫死寬')
  assert.doesNotMatch(desktopMain, /flex:[^;]*18em/, '不再把 18em 當固定寬（basis）')
  assert.match(desktopMain, /max-width:\s*18em;/, '名字很長時仍不超過 18em，讓額度列留得到寬')
})

test('名字按鈕與 pane id 不 grow：名字與 ★ 之間不留空（#1269）', () => {
  const name = head.match(/\.bot-head \.main-title-row > \.bot-name-input \{([^}]*)\}/)
  assert.ok(name, '名字規則要存在')
  assert.match(name[1], /flex:\s*0 1 auto;/, '名字 flex-grow 必須是 0')
  const pane = head.match(/^\.bot-head \.main-title-sub > \.main-status\.pane-toggle \{([^}]*)\}/m)
  assert.ok(pane, 'pane id 基本規則要存在')
  assert.match(pane[1], /flex:\s*0 1 auto;/, 'pane id 不 grow')
})
