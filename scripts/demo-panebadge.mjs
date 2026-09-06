// 側欄左上角的 pane 徽章：現在開著幾個 herdr pane（＝有幾個 bot 的終端還在）。
// 和 RAM 那格並排，標題縮寫成「AG Man」把寬度讓出來。
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5311 --strictPort`
// Usage: node scripts/demo-panebadge.mjs [http://127.0.0.1:5311/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9348
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-panebadge', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
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
// 只截側欄那一列：徽章的變化全在那裡。
const shot = async (name) => { await sleep(320); const { data } = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 460, height: 60, scale: 2 } }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const head = () => ev(`JSON.stringify({
  title: document.querySelector('.sidebar-head h1')?.textContent,
  full: document.querySelector('.sidebar-head h1')?.getAttribute('title'),
  pane: document.querySelector('.sidebar-head .pane-badge')?.textContent?.trim() ?? null,
  tip: document.querySelector('.sidebar-head .pane-badge')?.getAttribute('title')?.split('\\n').filter(Boolean) ?? null,
})`)
const startBot = async (name) => {
  await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>(r.querySelector('.bot-name')?.textContent||'').trim().startsWith(${JSON.stringify(name)}));if(!r)return 'MISSING';r.click();const b=[...r.querySelectorAll('button')].find(x=>x.title?.includes('啟動')||x.textContent.trim()==='啟動');if(b){b.click();return 'started row'}return 'no start button'})()`)
  await sleep(400)
  await ev(`[...document.querySelectorAll('button')].find((b) => b.textContent.trim() === '啟動')?.click()`)
  await sleep(2200)
}

console.log('== 沒有 bot 在跑：只有標題與連線燈 ==')
console.log(' ', await head())
await shot('354-panebadge-idle')

console.log('== 啟動兩個 bot ==')
await startBot('am-claude')
console.log(' ', await head())
await startBot('am-codex')
console.log(' ', await head())
await shot('355-panebadge-running')

console.log('== 正式模式的寬度（把 MOCK 徽章拿掉再量）==')
console.log(' ', await ev(`(()=>{document.querySelector('.sidebar-head .mock-badge')?.remove();const h=document.querySelector('.sidebar-head');return JSON.stringify({scroll:h.scrollWidth, client:h.clientWidth, overflow:h.scrollWidth>h.clientWidth})})()`))
await shot('356-panebadge-no-mock')

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 300))
ws.close(); chrome.kill(); process.exit(0)
