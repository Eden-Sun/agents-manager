// claude 的「預設」強度按鈕現在會提示帳號目前的設定（那個身份的 settings.json，SPEC §17.1），
// 換身份要跟著換數字：預設帳號 high（opus 被覆寫成 low）、cc1 只有 medium、cc2 什麼都沒設過
// （落回 claude 自己的內建預設 high，不是空白——官方文件 + 真機 /effort slider 都驗過）。
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5311 --strictPort`
// Usage: node scripts/demo-effort-default-hint.mjs [http://127.0.0.1:5311/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9354
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-effort-default', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
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
const defaultBtn = () => ev(`(()=>{const g=[...document.querySelectorAll('.bs-body .field')].find(f=>f.querySelector('.opt-group[aria-label="reasoning effort"]'));const b=g?.querySelector('.opt-group[aria-label="reasoning effort"] .opt');return b?{text:b.textContent.trim(), title:b.title}:null})()`)
const pickIdentity = (name) => ev(`(()=>{const b=[...document.querySelectorAll('.bs-body .opt-group[aria-label=identity] .opt')].find(x=>x.textContent.trim()===${JSON.stringify(name)});if(!b)return 'MISSING '+${JSON.stringify(name)};b.click();return 'picked '+${JSON.stringify(name)}})()`)

console.log('== 開 am-claude 的 Bot 設定 ==')
console.log(' ', await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>(r.querySelector('.bot-name')?.textContent||'').trim().startsWith('am-claude'));if(!r)return 'MISSING am-claude';r.click();const g=r.querySelector('.icon-btn.gear');if(!g)return 'no gear';g.click();return 'opened'})()`))
await sleep(700)

console.log('== 不指定身份（預設帳號）：sonnet 應該是 high ==')
console.log('  預設按鈕:', await defaultBtn())
await shot('362-effort-default-hint-none')

console.log('== 選 opus：帳號的 per-model 覆寫應該蓋過全域 high，變成 low ==')
console.log(' ', await ev(`(()=>{const b=[...document.querySelectorAll('.bs-body .opt-group[aria-label=model] .opt')].find(x=>x.textContent.trim()==='Opus');if(!b)return 'MISSING Opus';b.click();return 'picked opus'})()`))
await sleep(400)
console.log('  預設按鈕:', await defaultBtn())
await shot('363-effort-default-hint-opus-override')

console.log('== 切到 cc1（只有全域 medium，沒有 opus 覆寫）==')
console.log(' ', await pickIdentity('cc1'))
await sleep(600)
console.log('  預設按鈕:', await defaultBtn())
await shot('364-effort-default-hint-cc1')

console.log('== 切到 cc2（settings.json 兩個欄位都沒設過）==')
// 沒設過不等於沒有預設：claude 自己的內建預設是 high（官方文件 + 真機 /effort slider 都驗過），
// 這裡驗證的就是「沒有帳號層級的覆寫」時，daemon 老實回報那個內建值，不是空白。
console.log(' ', await pickIdentity('cc2'))
await sleep(600)
console.log('  預設按鈕:', await defaultBtn())
await shot('365-effort-default-hint-cc2')

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 300))
ws.close(); chrome.kill(); process.exit(0)
