// Verify TeamPanel's shared header flex contract in mock mode.
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5307 --strictPort`
// Run: `node scripts/verify-main-head-layout.mjs`
import { spawn } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const URL_BASE = process.env.DEMO_URL ?? 'http://127.0.0.1:5307/'
const OUT = resolve(dirname(fileURLToPath(import.meta.url)), '../docs/screenshots')
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9357
mkdirSync(OUT, { recursive: true })

const chrome = spawn(
  CHROME,
  [
    '--headless=new',
    `--remote-debugging-port=${PORT}`,
    '--disable-gpu',
    '--hide-scrollbars',
    '--no-first-run',
    '--user-data-dir=/tmp/am-cdp-main-head-layout',
    '--window-size=1440,860',
    URL_BASE,
  ],
  { stdio: 'ignore' },
)

const sleep = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms))
let ws
let id = 0
const pending = new Map()
const events = []
const send = (method, params = {}) => {
  const requestId = ++id
  ws.send(JSON.stringify({ id: requestId, method, params }))
  return new Promise((resolveSend, rejectSend) => pending.set(requestId, { resolve: resolveSend, reject: rejectSend }))
}

for (let i = 0; i < 80; i += 1) {
  try {
    const pages = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json()
    const page = pages.find((item) => item.type === 'page' && item.url.startsWith('http'))
    if (page) {
      ws = new WebSocket(page.webSocketDebuggerUrl)
      break
    }
  } catch {}
  await sleep(250)
}

if (!ws) throw new Error(`Chrome did not expose CDP on ${PORT}`)
ws.onmessage = (event) => {
  const message = JSON.parse(event.data)
  if (message.id && pending.has(message.id)) {
    const request = pending.get(message.id)
    pending.delete(message.id)
    if (message.error) request.reject(new Error(JSON.stringify(message.error)))
    else request.resolve(message.result)
  } else events.push(message)
}
await new Promise((resolveOpen) => (ws.onopen = resolveOpen))
await send('Runtime.enable')
await send('Page.enable')

const ev = async (expression) => {
  const result = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
  if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? 'Runtime evaluation failed')
  return result.result?.value
}

const metrics = async (width, height = 860) => {
  await send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 2, mobile: false })
  // QuotaStrip collapses off a `resize` listener; the metrics override alone does not
  // fire one in headless, so the strip would stay at its widest and skew every number.
  await ev("window.dispatchEvent(new Event('resize'))")
  await sleep(120)
}

const waitFor = async (selector) => {
  for (let i = 0; i < 80; i += 1) {
    if (await ev(`Boolean(document.querySelector(${JSON.stringify(selector)}))`)) return
    await sleep(100)
  }
  throw new Error(`Timed out waiting for ${selector}`)
}

const screenshot = async (name, width, height = 180) => {
  await sleep(350)
  const { data } = await send('Page.captureScreenshot', {
    format: 'png',
    clip: { x: 0, y: 0, width, height, scale: 2 },
  })
  writeFileSync(resolve(OUT, `${name}.png`), Buffer.from(data, 'base64'))
  console.log(`saved ${name}.png`)
}

const measure = (label, width) =>
  ev(`(() => {
    const rect = (selector) => {
      const el = document.querySelector(selector)
      if (!el) return null
      const r = el.getBoundingClientRect()
      const css = getComputedStyle(el)
      return {
        left: Number(r.left.toFixed(1)),
        right: Number(r.right.toFixed(1)),
        width: Number(r.width.toFixed(1)),
        minWidth: css.minWidth,
        flex: css.flex,
        overflow: css.overflow,
      }
    }
    return {
      state: ${JSON.stringify(label)},
      viewport: ${width},
      title: rect('.team-head .main-title strong'),
      cluster: rect('.team-head .main-title'),
      phase: rect('.team-head .team-phase'),
      deliver: rect('.team-head .team-deliver'),
      spacer: rect('.team-head > .spacer'),
      budget: rect('.team-head .team-budget-meter'),
      quota: rect('.team-head > .quota-strip'),
      actions: rect('.team-head > .head-actions'),
      mainHead: rect('.team-head'),
      scrollWidth: document.documentElement.scrollWidth,
      innerWidth: window.innerWidth,
    }
  })()`)

// Create one real mock TeamPanel, then replace only the rendered strings with the
// long issue/badge combination from the bug report. This exercises the production
// DOM/CSS contract while avoiding a permanent mock-data fixture.
await send('Page.navigate', { url: URL_BASE })
await sleep(2200)
await ev('localStorage.clear(); location.reload()')
await sleep(2200)
await ev("document.querySelector('.issues-btn')?.click()")
await waitFor('.issues-pop')
// The issue list loads asynchronously: wait for a row, not just the popup shell.
await waitFor('.issue-row .team-btn')
await ev("document.querySelector('.issue-row .team-btn')?.click()")
await waitFor('.team-launch')
await sleep(200)
await ev("[...document.querySelectorAll('button')].find((button) => button.textContent.includes('建立並啟動'))?.click()")
await waitFor('.team-head .team-phase')
await ev("window.__amMock?.teamPause('review_exhausted')")
await sleep(120)
await ev(`(() => {
  const title = document.querySelector('.team-head .main-title strong')
  const phase = document.querySelector('.team-head .team-phase')
  const deliver = document.querySelector('.team-head .team-deliver')
  if (!title || !phase || !deliver) throw new Error('Team badges are missing')
  title.textContent = 'Team · #1 群組時間軸的 ULID 同毫秒排序隱患（messages 可用 rowid 零成本…）'
  title.title = title.textContent
  phase.textContent = '已暫停・成員啟動失敗（i1-rev）'
  phase.title = phase.textContent
  deliver.textContent = '交付：留分支'
  deliver.title = deliver.textContent
  return 'scenario injected'
})()`)

// Recreate the pre-fix rules at the end of the current stylesheet for a true before
// capture. The rule is removed before the after measurements.
await ev(`(() => {
  const style = document.createElement('style')
  style.id = 'layout-baseline'
  style.textContent = ${JSON.stringify(`
    .main-head { min-width: auto !important; }
    .team-head .main-title { flex: none !important; min-width: 0 !important; }
    .team-head .main-title strong { flex: 0 1 auto !important; min-width: auto !important; }
    .team-head .team-phase,
    .team-head .team-deliver {
      flex: none !important;
      max-width: none !important;
      overflow: visible !important;
      text-overflow: clip !important;
    }
  `)}
  document.head.appendChild(style)
  return 'baseline rules installed'
})()`)

const rows = []
for (const width of [1440, 1100]) {
  await metrics(width)
  rows.push(await measure('before', width))
  await screenshot(`main-head-quota-clip-before${width === 1100 ? '-1100' : ''}`, width)
}

await ev("document.querySelector('#layout-baseline')?.remove()")
for (const width of [1800, 1600, 1440, 1280, 1100, 1099, 1024]) {
  await metrics(width)
  rows.push(await measure('after', width))
  if (width === 1440 || width === 1100) await screenshot(`main-head-quota-clip-after${width === 1100 ? '-1100' : ''}`, width)
}

console.log('MEASUREMENTS  state  vw | title | phase | deliver | quota.right | actions.right | overflow?')
for (const row of rows) {
  const w = (r) => (r ? Math.round(r.width) : '-')
  const over = Math.max(row.quota?.right ?? 0, row.actions?.right ?? 0) - row.innerWidth
  console.log(
    `${row.state.padEnd(6)} ${String(row.viewport).padStart(4)} | title ${String(w(row.title)).padStart(4)}` +
      ` | phase ${String(w(row.phase)).padStart(4)} | deliver ${String(w(row.deliver)).padStart(3)}` +
      ` | quota ${String(w(row.quota)).padStart(3)}@${Math.round(row.quota?.right ?? 0)}` +
      ` | actions@${Math.round(row.actions?.right ?? 0)}` +
      ` | overflow ${over > 0 ? `+${Math.round(over)}px` : 'none'}` +
      ` | hscroll ${row.scrollWidth > row.innerWidth ? 'YES' : 'no'}` +
      ` | badgeEscape ${row.cluster && row.deliver && row.deliver.right > row.cluster.right + 1 ? 'YES' : 'no'}`,
  )
}

// Shared-header regression screenshots: GroupChatPanel and ChatPanel keep their
// existing title/spacer/actions composition because the Team overrides are scoped.
await metrics(1440)
await ev("document.querySelector('.project-label-btn')?.click()")
await waitFor('.group-head')
await screenshot('main-head-group-chat-after', 1440)
await ev("document.querySelector('.bot-row')?.click()")
await waitFor('.main-head:not(.group-head):not(.team-head)')
await screenshot('main-head-chat-after', 1440)

console.log('--- console/runtime errors ---')
for (const event of events) {
  if (event.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(event.params).slice(0, 500))
  if (event.method === 'Runtime.consoleAPICalled' && event.params.type === 'error') console.log(JSON.stringify(event.params.args).slice(0, 500))
}

ws.close()
chrome.kill()
