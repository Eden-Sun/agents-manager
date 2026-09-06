// Team 的成員也要看得見身分（cc0 / cc1 / cc2）：一個 team 常常一個角色一個帳號，
// 側欄的 team 節點與主區的成員 chip 都要標出來，不能只留在 tooltip。
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5311 --strictPort`
// Usage: node scripts/demo-team-identity.mjs [http://127.0.0.1:5311/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9349
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-team-ident', '--window-size=1440,960', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 960, deviceScaleFactor: 2, mobile: false })
await send('Page.navigate', { url: URL_BASE }); await sleep(2400)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name, w = 1440, h = 700) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: w, height: h, scale: 2 } }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const waitFor = async (sel, tries = 60) => { for (let i = 0; i < tries; i++) { if (await ev(`Boolean(document.querySelector(${JSON.stringify(sel)}))`)) return true; await sleep(250) } return false }

console.log('== 開組隊表單，給 PM 指定 cc2、執行者留預設 ==')
// Issues 列掛在對話面板底下，所以先選一個 bot（乾淨的 profile 沒有記住上次選誰）。
await ev("document.querySelector('.bot-row')?.click()")
await waitFor('.issues-btn')
await ev("document.querySelector('.issues-btn')?.click()")
await waitFor('.issue-row .team-btn')
// issue 清單是非同步載進來的：太早點會拿到 #0（mock 回 `gh: issue #0 not found`）。
await sleep(1200)
console.log('  第一筆 issue:', await ev(`document.querySelector('.issue-row')?.textContent.trim().slice(0,40)`))
await ev("document.querySelector('.issue-row .team-btn')?.click()")
await waitFor('.team-launch')
await sleep(500)
console.log('  PM 身分選項:', await ev(`[...document.querySelectorAll('.team-card')][0]?.querySelectorAll('.opt-group[aria-label=identity] .opt').length ?? 0`))
console.log(' ', await ev(`(()=>{const card=[...document.querySelectorAll('.team-card')][0];const b=[...(card?.querySelectorAll('.opt-group[aria-label=identity] .opt')??[])].find(x=>x.textContent.trim()==='cc2');if(!b)return 'MISSING cc2';b.click();return 'PM = cc2'})()`))
await sleep(300)

console.log('== 建立 team ==')
console.log(' ', await ev(`(()=>{const b=[...document.querySelectorAll('.team-launch button')].find(x=>x.textContent.trim().includes('建立'));if(!b)return 'MISSING 建立';if(b.disabled)return 'disabled';b.click();return 'created'})()`))
// 建立後 UI 會跳到 team 面板；成員是 daemon（mock）啟動出來的，等它們冒出來。
await sleep(2000)
console.log('  主區:', await ev(`[...document.querySelectorAll('.main > *')].map(e=>e.className).slice(0,4).join(' | ')`))
console.log('  側欄 team 節點:', await ev(`[...document.querySelectorAll('.team-node, .team-row, [class*=team]')].map(e=>e.className).slice(0,8).join(' | ')`))
console.log('  notices:', await ev(`[...document.querySelectorAll('.notice')].map(n=>n.textContent.trim().slice(0,90)).join(' | ')`))
const gotMembers = await waitFor('.team-members .team-member', 160)
console.log('  members appeared:', gotMembers)
if (!gotMembers) {
  console.log('  側欄 team 節點文字:', await ev(`[...document.querySelectorAll('.sidebar [class*=team]')].map(e=>e.className+':'+e.textContent.trim().slice(0,24)).slice(0,10).join(' | ')`))
  // 側欄的 team 節點要先點開才會列出成員。
  console.log(' ', await ev(`(()=>{const n=document.querySelector('.sidebar .team-node, .sidebar [class*=team-head]');if(!n)return 'no team node';n.click();return 'clicked team node'})()`))
  await sleep(1500)
  await waitFor('.team-members .team-member', 60)
}
await sleep(1500)

console.log('== 成員 chip 與側欄節點都要標身分 ==')
console.log('  主區 chip:', await ev(`[...document.querySelectorAll('.team-members .team-member')].map(m=>m.textContent.trim().replace(/\\s+/g,' ')).join(' | ')`))
console.log('  側欄成員:', await ev(`[...document.querySelectorAll('.team-member-row')].map(r=>r.querySelector('.bot-name')?.textContent.trim()+' → '+[...r.querySelectorAll('.bot-sub .identity-badge')].map(b=>b.textContent.trim()).join(',')).join(' | ')`))
console.log('  chip tooltip:', await ev(`document.querySelector('.team-members .team-member')?.title?.split('\\n').filter(l=>l.includes('身分')).join('')`))
await shot('357-team-member-identity')

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 300))
ws.close(); chrome.kill(); process.exit(0)
