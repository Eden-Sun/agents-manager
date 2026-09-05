import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = 'http://127.0.0.1:7788/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(CHROME, ['--headless=new','--remote-debugging-port=9337','--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-picker','--window-size=1280,860', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch('http://127.0.0.1:9337/json/list')).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1280,height:860,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2500)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(300); const { data } = await send('Page.captureScreenshot',{format:'png'}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
const clickText = (t, sel='button') => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim().includes(${JSON.stringify(t)}));if(!e)return 'missing '+${JSON.stringify(t)};e.click();return 'clicked '+${JSON.stringify(t)}})()`)
console.log(await clickText('新增 Project')); await sleep(400)
console.log(await clickText('瀏覽')); await sleep(1200)
console.log('picker path:', await ev(`document.querySelector('.dirpicker-cur')?.textContent`), '| rows:', await ev(`document.querySelectorAll('.dirpicker-row').length`))
await shot('40-picker-home')
console.log(await clickText('project', '.dirpicker-row')); await sleep(1000)
console.log('picker path:', await ev(`document.querySelector('.dirpicker-cur')?.textContent`), '| rows:', await ev(`document.querySelectorAll('.dirpicker-row').length`))
await shot('41-picker-project-dir')
console.log(await clickText('chowface', '.dirpicker-row')); await sleep(1000)
console.log('picker path:', await ev(`document.querySelector('.dirpicker-cur')?.textContent`))
console.log(await clickText('選擇此目錄')); await sleep(500)
console.log('form path:', await ev(`document.querySelector('.form input[type=text]')?.value`), '| label:', await ev(`document.querySelectorAll('.form input[type=text]')[1]?.value`))
await shot('42-picker-selected')
console.log('--- errors ---')
for (const e of events) if (e.method==='Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,400))
ws.close(); chrome.kill(); process.exit(0)
