// ChatPanel header: the repo chip + status line now share one row (`.context-bar`).
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.DEMO_URL ?? 'http://127.0.0.1:5411/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9371
const chrome = spawn(CHROME, ['--headless=new',`--remote-debugging-port=${PORT}`,'--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-contextbar','--window-size=1440,860', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1440,height:860,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2600)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? 'eval failed'); return r.result?.value }
const shot = async (name, height = 200) => { await sleep(350); const { data } = await send('Page.captureScreenshot',{format:'png',clip:{x:0,y:0,width:1440,height,scale:2}}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }

// A claude bot: it is the kind that ships a real statusLine payload. It has to be running
// for the payload to exist, so start it and wait for the mock's `starting -> running` hop.
await ev("[...document.querySelectorAll('.bot-row')].find((r) => r.textContent.includes('am-claude'))?.click()")
await sleep(700)
await ev("[...document.querySelectorAll('.main-head button, .bot-stopped-bar button')].find((b) => b.textContent.trim() === '啟動')?.click()")
for (let i = 0; i < 60; i += 1) {
  if (await ev("Boolean(document.querySelector('.statusline-bar .sl-k'))")) break
  await sleep(250)
}
await sleep(400)

const rows = () => ev(`(() => {
  const chrome = ['.main-head', '.context-bar', '.statusline-bar', '.issues-bar', '.run-debug-bar']
  const seen = []
  for (const sel of chrome) for (const el of document.querySelectorAll(sel)) {
    const r = el.getBoundingClientRect()
    if (r.height > 0 && !el.closest('.context-bar > *:not(:scope)')) seen.push({ sel, top: Math.round(r.top), h: Math.round(r.height) })
  }
  const head = document.querySelector('.main-head').getBoundingClientRect()
  const chat = document.querySelector('.chat, .msg-list')?.getBoundingClientRect() ?? null
  return {
    bars: seen.filter((b) => b.sel !== '.statusline-bar' || !document.querySelector('.context-bar > .statusline-bar')),
    chromeHeight: chat ? Math.round(chat.top - head.top) : null,
    fields: [...document.querySelectorAll('.statusline-bar .sl-k')].map((e) => e.textContent),
    modelBadge: document.querySelector('.main-head .model-tag')?.textContent ?? null,
    botName: (() => {
      const el = document.querySelector('.main-head .main-title > strong')
      return el ? \`\${el.textContent} @\${Math.round(el.getBoundingClientRect().width)}px\` : null
    })(),
    gearVisible: (() => {
      const g = document.querySelector('.main-head .icon-btn.gear')
      if (!g) return false
      const r = g.getBoundingClientRect()
      return r.width > 8 && r.right <= window.innerWidth
    })(),
    quotaRight: Math.round(document.querySelector('.main-head > .quota-strip')?.getBoundingClientRect().right ?? 0),
    actionsRight: Math.round(document.querySelector('.main-head > .head-actions')?.getBoundingClientRect().right ?? 0),
    innerWidth: window.innerWidth,
    issuesInContextBar: Boolean(document.querySelector('.context-bar > .issues-bar')),
    statusInContextBar: Boolean(document.querySelector('.context-bar > .statusline-bar')),
  }
})()`)

for (const w of [1920, 1440, 1280]) {
  await send('Emulation.setDeviceMetricsOverride', { width: w, height: 860, deviceScaleFactor: 2, mobile: false })
  await ev("window.dispatchEvent(new Event('resize'))")
  await sleep(250)
  console.log(`AFTER @${w}`, JSON.stringify(await rows()))
  if (w === 1920) {
    await sleep(300)
    const { data } = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1920, height: 200, scale: 2 } })
    writeFileSync(`${OUT}/329-contextbar-after-1920.png`, Buffer.from(data, 'base64'))
    console.log('saved 329-contextbar-after-1920')
  }
}
await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 860, deviceScaleFactor: 2, mobile: false })
await sleep(250)
await shot('330-contextbar-after')

// The issues popup must not be clipped by the row's own scrolling half.
await ev("document.querySelector('.context-bar .issues-btn')?.click()")
await sleep(700)
console.log('issues popup:', await ev(`(() => {
  const pop = document.querySelector('.issues-pop')
  if (!pop) return 'MISSING'
  const r = pop.getBoundingClientRect()
  const bar = document.querySelector('.context-bar').getBoundingClientRect()
  return \`h=\${Math.round(r.height)} bottom=\${Math.round(r.bottom)} barBottom=\${Math.round(bar.bottom)} clipped=\${r.height < 40}\`
})()`))
await shot('331-contextbar-issues-open', 520)
await ev("document.querySelector('.context-bar .issues-btn')?.click()"); await sleep(300)

// Terminal tab: no composer to insert an issue into, so the chip steps aside.
await ev("[...document.querySelectorAll('.tab')].find((t) => t.textContent.includes('終端'))?.click()")
await sleep(700)
console.log('terminal tab:', JSON.stringify(await rows()))
await shot('332-contextbar-terminal')

console.log('--- errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,400))
ws.close(); chrome.kill(); process.exit(0)
