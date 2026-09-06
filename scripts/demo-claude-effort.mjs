// claude 的 `--effort`（2.1+）：Bot 設定的「強度」列現在對 claude 也要出現，
// 而且改了要回「重啟後才會套用」——claude 的 `/effort` 是拉桿，沒有帶參數的 slash 指令。
// 走 mock backend（`VITE_MOCK=1 npx vite --port 5311`）。
// Usage: node scripts/demo-claude-effort.mjs [http://127.0.0.1:5311/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9347
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-claude-effort', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 2, mobile: false })
await send('Page.navigate', { url: URL_BASE }); await sleep(2200)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const opts = (label) => ev(`[...document.querySelectorAll('.bs-body .opt-group[aria-label=${JSON.stringify(label)}] .opt')].map(b=>b.textContent.trim()).join(' | ')`)

console.log('== claude bot 的設定面板 ==')
console.log(' ', await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>(r.querySelector('.bot-name')?.textContent||'').trim().startsWith('am-claude'));if(!r)return 'MISSING am-claude';r.click();const g=r.querySelector('.icon-btn.gear');if(!g)return 'no gear';g.click();return 'opened'})()`))
await sleep(700)
console.log('  模型:', await opts('model'))
console.log('  強度:', await opts('reasoning effort'))
console.log(' ', await ev(`(()=>{const b=[...document.querySelectorAll('.bs-body .opt-group[aria-label="reasoning effort"] .opt')].find(x=>x.textContent.trim()==='高');if(!b)return 'MISSING 高';b.click();return 'picked 高'})()`))
await sleep(300)
console.log('  儲存前提示:', await ev(`document.querySelector('.bs-foot .hint, .bs-foot span')?.textContent`))
await shot('353-claude-effort')

console.log('== codex 仍有它自己的 8 級、grok 4 級 ==')
for (const name of ['am-codex', 'am-grok']) {
  console.log(' ', await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>(r.querySelector('.bot-name')?.textContent||'').trim().startsWith(${JSON.stringify(name)}));if(!r)return 'MISSING';r.click();const g=r.querySelector('.icon-btn.gear');if(!g)return 'no gear';g.click();return 'opened '+${JSON.stringify(name)}})()`))
  await sleep(700)
  console.log(`  ${name} 強度:`, await opts('reasoning effort'))
  await ev(`document.querySelector('.bs-foot button')?.click()`)
  await sleep(300)
}

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 300))
ws.close(); chrome.kill(); process.exit(0)
