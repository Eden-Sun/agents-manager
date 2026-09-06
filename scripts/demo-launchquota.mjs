// 組隊（TeamLaunchPanel）header must show the quota strip too.
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.DEMO_URL ?? 'http://127.0.0.1:5411/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9379
const chrome = spawn(CHROME, ['--headless=new',`--remote-debugging-port=${PORT}`,'--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-launchquota','--window-size=1600,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1600,height:900,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2600)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? 'eval failed'); return r.result?.value }
const shot = async (name, h = 120) => { await sleep(320); const { data } = await send('Page.captureScreenshot',{format:'png',clip:{x:0,y:0,width:1600,height:h,scale:2}}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
const waitFor = async (sel) => { for (let i=0;i<80;i++){ if (await ev(`Boolean(document.querySelector(${JSON.stringify(sel)}))`)) return; await sleep(120) } throw new Error(`timeout ${sel}`) }

const head = () => ev(`(() => {
  const h = document.querySelector('.team-launch-head')
  const q = document.querySelector('.team-launch-head > .quota-strip')
  const acts = document.querySelector('.team-launch-head > .head-actions')
  return JSON.stringify({
    isLaunch: Boolean(h),
    gauges: document.querySelectorAll('.team-launch-head .quota-hp').length,
    quotaRight: q ? Math.round(q.getBoundingClientRect().right) : null,
    actionsRight: acts ? Math.round(acts.getBoundingClientRect().right) : null,
    innerWidth: window.innerWidth,
    hscroll: document.documentElement.scrollWidth > window.innerWidth,
    previewCard: Boolean(document.querySelector('.team-quota, .team-quota-row')),
    cards: [...document.querySelectorAll('.team-launch .team-card-title')].map((e) => e.textContent.trim()).join(' | '),
    blocks: document.querySelectorAll('.team-launch-blocks .team-block').length,
  })
})()`)

await ev("document.querySelector('.issues-btn')?.click()")
await waitFor('.issue-row .team-btn')
await ev("document.querySelector('.issue-row .team-btn')?.click()")
await waitFor('.team-launch')
await sleep(400)
console.log('組隊 header |', await head())
await shot('370-launch-header-quota', 700)

// The two *blocking* alerts survived the card removal. `missingCli` cannot be reached from
// the UI (an uninstalled CLI is a disabled option), so drive the other one: drop the quota
// stop line under what the mock has already used and the warning must appear — right above
// the action buttons, which is where the removed card used to sit.
console.log('kind opts   |', await ev(`[...document.querySelectorAll('.team-card .opt')].slice(0, 3).map((b) => b.textContent.trim().slice(0, 10) + (b.disabled ? '[disabled]' : '')).join(' , ')`))
await ev(`(() => {
  const field = [...document.querySelectorAll('.team-budget-field')].find((f) => f.textContent.includes('停手線'))
  const input = field?.querySelector('input')
  if (!input) return 'no stop-line input'
  const set = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set
  set.call(input, '1')
  input.dispatchEvent(new Event('input', { bubbles: true }))
  return 'stop line = 1%'
})()`)
await sleep(500)
console.log('stop line 1%|', await ev(`JSON.stringify({
  blocks: document.querySelectorAll('.team-launch-blocks .team-block').length,
  text: document.querySelector('.team-launch-blocks .team-block')?.textContent?.trim().slice(0, 40) ?? null,
  aboveActions: (() => {
    const b = document.querySelector('.team-launch-blocks')
    const a = document.querySelector('.team-launch-actions')
    return Boolean(b && a && b.getBoundingClientRect().bottom <= a.getBoundingClientRect().top + 1)
  })(),
})`))
await shot('371-launch-block-alert', 700)

for (const w of [1280, 1100]) {
  await send('Emulation.setDeviceMetricsOverride', { width: w, height: 900, deviceScaleFactor: 2, mobile: false })
  await ev("window.dispatchEvent(new Event('resize'))"); await sleep(250)
  console.log(`@${w}       |`, await head())
}

console.log('--- errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,400))
ws.close(); chrome.kill(); process.exit(0)
