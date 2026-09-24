/**
 * issue #545：blocked 全畫面視窗是自己彈出來的，鍵盤直通不能預設開著——尤其是 #423 的防誤刪框，
 * 打字打到一個 `1` 就等於在 TUI 上按下「1. Yes」。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { blockedKeyAction, passthroughLive, type BlockedKeyContext } from './blockedKeys.ts'
import { isDangerousRmScreen } from './dangerousRm.ts'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

const ctx = (over: Partial<BlockedKeyContext> = {}): BlockedKeyContext => ({
  passthrough: false,
  dangerous: false,
  inside: false,
  tag: 'body',
  key: '1',
  defaultPrevented: false,
  ...over,
})

test('直通關著時打字不會送進 pane，Esc 用來關視窗', () => {
  assert.equal(blockedKeyAction(ctx({ key: '1' })), 'browser')
  assert.equal(blockedKeyAction(ctx({ key: 'y' })), 'browser')
  assert.equal(blockedKeyAction(ctx({ key: 'Enter' })), 'browser')
  assert.equal(blockedKeyAction(ctx({ key: 'Escape' })), 'close')
  assert.equal(blockedKeyAction(ctx({ key: 'Escape', defaultPrevented: true })), 'browser')
})

test('直通打開才送進 pane；Esc 也送進去（所以關閉只剩 ✕）', () => {
  assert.equal(blockedKeyAction(ctx({ passthrough: true, key: '1' })), 'pane')
  assert.equal(blockedKeyAction(ctx({ passthrough: true, key: 'Escape' })), 'pane')
})

test('防誤刪框：就算直通打開也一律不送進 pane（#423：只有使用者本人能核准）', () => {
  assert.equal(passthroughLive(true, true), false)
  assert.equal(blockedKeyAction(ctx({ passthrough: true, dangerous: true, key: '1' })), 'browser')
  assert.equal(blockedKeyAction(ctx({ passthrough: true, dangerous: true, key: 'Enter' })), 'browser')
  // 這時 Esc 回到「關視窗」，人才有路走。
  assert.equal(blockedKeyAction(ctx({ passthrough: true, dangerous: true, key: 'Escape' })), 'close')
})

test('視窗裡的輸入框與按鈕照舊留住鍵盤，不被直通吃掉', () => {
  for (const tag of ['input', 'textarea', 'select']) {
    assert.equal(blockedKeyAction(ctx({ passthrough: true, inside: true, tag, key: 'a' })), 'browser', tag)
  }
  assert.equal(blockedKeyAction(ctx({ passthrough: true, inside: true, tag: 'button', key: 'Enter' })), 'browser')
  assert.equal(blockedKeyAction(ctx({ passthrough: true, inside: true, tag: 'button', key: ' ' })), 'browser')
  assert.equal(blockedKeyAction(ctx({ passthrough: true, key: 'Tab' })), 'browser')
  // 按鈕上的數字鍵仍然是直通（那是人在答題），只有 Enter／Space 留給按鈕。
  assert.equal(blockedKeyAction(ctx({ passthrough: true, inside: true, tag: 'button', key: '1' })), 'pane')
})

// ── 偵測：拿 daemon 的真畫面 fixture 對，兩邊看的是同一張圖 ──

const fixture = (name: string) =>
  readFileSync(join(import.meta.dirname, '../../../daemon/src/lifecycle/fixtures', name), 'utf8')

test('#423 的真畫面認得出來；一般畫面與引用原文不算', () => {
  assert.equal(isDangerousRmScreen(fixture('claude-2.1.281-dangerous-rm.txt')), true)
  assert.equal(isDangerousRmScreen(fixture('claude-2.1.281-dangerous-rm-one-row.txt')), true)
  // 框已經自動拒絕、畫面回到對話：不該再鎖著。
  assert.equal(isDangerousRmScreen(fixture('claude-2.1.281-dangerous-rm-auto-denied.txt')), false)
  assert.equal(isDangerousRmScreen(null), false)
  assert.equal(isDangerousRmScreen(''), false)
  // 只有問句沒有警語（別的確認框）不算——會被鎖死就沒人能用直通了。
  assert.equal(isDangerousRmScreen('Do you want to proceed?\n❯ 1. Yes\n  2. No\n'), false)
  // 只有警語沒有選項也不算。
  assert.equal(isDangerousRmScreen('│ Dangerous rm operation on target: x\nsomething else\n'), false)
})
