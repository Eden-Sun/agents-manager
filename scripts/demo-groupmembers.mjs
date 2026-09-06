// Group header: members are icon-only now, and the attach command lives here only.
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.DEMO_URL ?? 'http://127.0.0.1:5411/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9375
const chrome = spawn(CHROME, ['--headless=new',`--remote-debugging-port=${PORT}`,'--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-groupmembers','--window-size=1440,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1440,height:900,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2600)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? 'eval failed'); return r.result?.value }
const shot = async (name, w = 1440, h = 130) => { await sleep(320); const { data } = await send('Page.captureScreenshot',{format:'png',clip:{x:0,y:0,width:w,height:h,scale:2}}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }

// bot view first: the attach button must be gone from it
await ev("document.querySelector('.bot-row')?.click()"); await sleep(700)
console.log('bot view   | attach button:', await ev("Boolean(document.querySelector('.main-head .attach'))"))

// group view
await ev("document.querySelector('.project-head strong, .project-head .project-label, .project-head')?.click()")
await sleep(900)
console.log('group view |', await ev(`(() => {
  const strip = document.querySelector('.group-head .members')
  const chips = [...document.querySelectorAll('.group-head .member')]
  return JSON.stringify({
    isGroup: Boolean(document.querySelector('.group-head')),
    memberChips: chips.length,
    chipText: chips.map((c) => c.textContent.trim()).join('|') || '(icons only)',
    chipsWidthEach: chips.length ? Math.round(chips[0].getBoundingClientRect().width) : null,
    chipTitles: chips.map((c) => c.getAttribute('title')?.split('（')[0]).join(', '),
    stripWidth: strip ? Math.round(strip.getBoundingClientRect().width) : null,
    countLabel: document.querySelector('.group-head .members-count')?.textContent?.trim() ?? null,
    attachButton: Boolean(document.querySelector('.group-head .attach')),
  })
})()`))
await shot('350-group-members-icons')

// clicking an icon still opens that bot's own chat
await ev("document.querySelector('.group-head .member')?.click()"); await sleep(800)
console.log('after chip click |', await ev(`JSON.stringify({
  stillGroup: Boolean(document.querySelector('.group-head')),
  headClasses: document.querySelector('.main-head')?.className ?? null,
  title: document.querySelector('.main-head strong')?.textContent ?? null,
})`))

console.log('--- errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,400))
ws.close(); chrome.kill(); process.exit(0)
