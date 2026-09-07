// 每個強度按鈕現在都會標出原廠推薦值（不只 tooltip，按鈕上直接掛「廠推薦」），
// 三種 kind（claude / codex / grok）都要看得到，不是只有 claude。
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5311 --strictPort`
// Usage: node scripts/demo-effort-mark.mjs [http://127.0.0.1:5311/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9355
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-effort-mark', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 2, mobile: false })
await send('Page.navigate', { url: URL_BASE }); await sleep(2400)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const marks = () => ev(`[...document.querySelectorAll('.bs-body .opt-group[aria-label="reasoning effort"] .opt')].map(b=>b.textContent.trim()).join(' | ')`)
const openSettings = (name) => ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>(r.querySelector('.bot-name')?.textContent||'').trim().startsWith(${JSON.stringify(name)}));if(!r)return 'MISSING';r.click();const g=r.querySelector('.icon-btn.gear');if(!g)return 'no gear';g.click();return 'opened'})()`)
const closeSettings = () => ev(`document.querySelector('.bs-foot button')?.click()`)

for (const [name, label] of [['am-claude', 'claude'], ['am-codex', 'codex'], ['am-grok', 'grok']]) {
  console.log(`== ${label} 的強度按鈕 ==`)
  console.log(' ', await openSettings(name))
  await sleep(700)
  console.log('  按鈕文字:', await marks())
  await shot(`366-effort-mark-${label}`)
  await closeSettings()
  await sleep(300)
}

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 300))
ws.close(); chrome.kill(); process.exit(0)
