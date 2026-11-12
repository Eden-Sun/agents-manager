/**
 * 缺工具安裝引導（ToolInstallGuide）與黃字提醒說明（BaselineLine）：
 * - 依 OS 只列對應指令；rtk 沒有指令就說明而不是猜
 * - 重新檢查按鈕會呼叫 onRecheck，執行中停用
 * - 黃字項附一句說明
 * - HostsPanel 的 placeholder 是通用範例，不是擁有者的私人主機
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll, act } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import { BaselineLine, HostsPanel } from './HostsPanel'
import { ToolInstallGuide } from './ToolInstallGuide'
import type { HostBaseline } from '../api/types'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 10_000 }, fn)

const base = (os: string, issues: HostBaseline['issues']): HostBaseline => ({ issues, checked_at: new Date().toISOString(), os, stale: false, error: null })

it('Linux 上缺 rtk：沒有指令、說明 PATH，仍有重新檢查', async () => {
  await mount(<ToolInstallGuide tool="rtk" os="Linux" onRecheck={async () => undefined} />)
  const root = document.querySelector('.tool-install-guide')!
  assert.ok(root)
  assert.equal(root.querySelectorAll('.tool-install-cmd').length, 0)
  assert.match(root.textContent ?? '', /沒有可自動核對的安裝指令/)
  assert.match(root.textContent ?? '', /登入 shell 的 PATH/)
  assert.equal(root.querySelector('a'), null, 'rtk 沒有官方連結就不畫')
  assert.ok(root.querySelector('button')?.textContent?.includes('重新檢查'))
})

it('macOS 上缺 gh：只列 brew 指令（可複製）與官方連結', async () => {
  await mount(<ToolInstallGuide tool="gh" os="Darwin" onRecheck={async () => undefined} />)
  const cmds = [...document.querySelectorAll('.tool-install-cmd code')].map((c) => c.textContent)
  assert.deepEqual(cmds, ['brew install gh'])
  assert.equal(document.querySelector<HTMLAnchorElement>('.tool-install-guide a')?.href, 'https://cli.github.com/')
  const copy = [...document.querySelectorAll('.tool-install-cmd button')]
  assert.equal(copy.length, 1)
})

it('重新檢查：呼叫 onRecheck，執行中停用，完成後恢復', async () => {
  let calls = 0
  let release!: () => void
  const onRecheck = () => {
    calls++
    return new Promise<void>((resolve) => {
      release = resolve
    })
  }
  await mount(<ToolInstallGuide tool="bun" os="Linux" onRecheck={onRecheck} />)
  const btn = () => [...document.querySelectorAll<HTMLButtonElement>('.tool-install-guide button')].find((b) => b.textContent?.includes('重新檢查') || b.textContent?.includes('檢查中'))!
  await click(btn())
  assert.equal(calls, 1)
  assert.equal(btn().disabled, true)
  assert.match(btn().textContent ?? '', /檢查中/)
  await act(async () => {
    release()
  })
  await settle()
  assert.equal(btn().disabled, false)
})

it('一致性區塊：缺工具那項內嵌引導；黃字項附說明', async () => {
  const b = base('Linux', [
    { id: 'tool.gh', severity: 'critical', message: '缺工具 gh（登入 shell 的 PATH 找不到）' },
    { id: 'codex.hooks.json', severity: 'warn', message: 'codex：缺 hooks.json' },
  ])
  await mount(<BaselineLine baseline={b} />)
  const critLi = document.querySelector('ul.host-baseline-items li.critical')!
  assert.ok(critLi.querySelector('.tool-install-guide'), '嚴重的缺工具項有引導')
  assert.equal(critLi.querySelectorAll('.tool-install-cmd code')[0]?.textContent, 'sudo apt install gh')
  const warnLi = document.querySelector('li.warn')!
  assert.ok(warnLi.querySelector('.host-baseline-why'), '黃字項有說明')
  assert.match(warnLi.querySelector('.host-baseline-why')?.textContent ?? '', /hook/)
})

it('HostsPanel 新增主機欄位的 placeholder 是通用範例，不是私人主機名', async () => {
  resetStoreForTest()
  await mount(<HostsPanel />)
  await settle()
  const phs = [...document.querySelectorAll<HTMLInputElement>('.hosts-panel input')].map((i) => i.placeholder)
  assert.ok(phs.includes('my-server'))
  assert.ok(phs.includes('user@my-server'))
  assert.equal(phs.some((p) => p.includes('mac_builder') || p.includes('100.64.0.2')), false)
})

it('選用工具（warn）：展開提醒後寫「選用：用途」，並仍有安裝引導與重新檢查；critical 工具沒有選用字樣', async () => {
  const baseline: HostBaseline = {
    issues: [
      { id: 'tool.herdr', severity: 'critical', message: '缺工具 herdr（登入 shell 的 PATH 找不到）' },
      { id: 'tool.bun', severity: 'warn', message: '缺工具 bun（登入 shell 的 PATH 找不到）', note: '只有從原始碼建置網頁介面才需要' },
    ],
    checked_at: new Date().toISOString(),
    os: 'Linux',
    stale: false,
    error: null,
  }
  await mount(<BaselineLine baseline={baseline} />)
  const crit = document.querySelector('ul.host-baseline-items li')!
  assert.doesNotMatch(crit.textContent ?? '', /選用：/)
  const fold = document.querySelector<HTMLDetailsElement>('details.env-fold')!
  assert.ok(fold, 'warn 項收在可展開的區塊')
  const li = fold.querySelector('li.warn')!
  assert.match(li.querySelector('.host-baseline-why')?.textContent ?? '', /^選用：只有從原始碼建置網頁介面才需要$/)
  assert.ok(li.querySelector('.tool-install-guide'), 'warn 等級的工具也有安裝引導')
  assert.ok([...li.querySelectorAll('button')].some((b) => b.textContent?.includes('重新檢查')))
})
