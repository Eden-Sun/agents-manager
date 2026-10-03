/**
 * 分享頁的 mock（VITE_MOCK=1）：跟 `httpShareClient` 同一個介面，資料只在記憶體。
 * 開法：`VITE_MOCK=1 npx vite` 後開 `/share.html?token=demo_share_token_0123456789`；token 換成 `expired_…` 開頭演「連結已失效」。
 */
import type { ShareClient, ShareEvents } from './shareApi'
import { ShareHttpError, SHARE_FILE_MAX, SHARE_TEXT_MAX, type ShareFile, type ShareMessage, type ShareStatus } from './shareModel'

const ago = (min: number) => new Date(Date.now() - min * 60_000).toISOString()
let n = 0
const id = () => `msg_mock_${Date.now().toString(36)}_${(n++).toString(36)}`

export function mockShareClient(token: string): ShareClient {
  const expired = token.startsWith('expired_')
  const messages: ShareMessage[] = [
    { id: 'm1', role: 'assistant', text: '你好，我是產品客服助理，可以回答安裝與帳號相關的問題，也可以看你上傳的設定檔。', created_at: ago(42), attachments: [] },
    { id: 'm2', role: 'user', text: '我的 config.toml 裝完之後一直說 port 被佔用，幫我看一下', created_at: ago(40), attachments: [{ name: 'config.toml' }] },
    {
      id: 'm3',
      role: 'assistant',
      text: '看了你的設定：`listen = "127.0.0.1:7788"` 跟另一個服務撞了。\n\n1. 改成 `7789`\n2. 重開服務\n\n改好的檔放在「bot 給你的檔案」裡，直接下載覆蓋就好。',
      created_at: ago(39),
      attachments: [],
    },
  ]
  const files: (ShareFile & { body: string })[] = [
    { name: 'config.toml', size: 418, modified_at: ago(39), body: 'listen = "127.0.0.1:7789"\n' },
    { name: '安裝步驟.md', size: 2_310, modified_at: ago(120), body: '# 安裝步驟\n' },
  ]
  let status: ShareStatus = 'idle'
  let replying = false
  let subs: ShareEvents[] = []
  const guard = async () => {
    await new Promise((r) => setTimeout(r, 120))
    if (expired) throw new ShareHttpError(404)
  }
  const push = (m: ShareMessage) => {
    messages.push(m)
    for (const s of subs) s.onMessage(m)
  }
  const setStatus = (s: ShareStatus) => {
    status = s
    for (const x of subs) x.onStatus(s)
  }
  const uploads = new Map<string, string>()

  // 截圖用：`__shareMock.thinking()` 讓 bot 停在「思考中」。
  ;(globalThis as Record<string, unknown>).__shareMock = { thinking: () => setStatus('working'), idle: () => setStatus('idle') }

  return {
    async messages() {
      await guard()
      return { bot_name: 'support-bot', status, messages: [...messages], has_more: false }
    },
    async send(text, _crid, attachments) {
      await guard()
      if (text.length > SHARE_TEXT_MAX) throw new ShareHttpError(413)
      if (text.includes('429')) throw new ShareHttpError(429, 30)
      // 同 daemon：一段對話同時只排一則，上一則還沒回完就 409 not_accepted。
      if (status === 'working' || replying) throw new ShareHttpError(409)
      replying = true
      push({ id: id(), role: 'user', text, created_at: new Date().toISOString(), attachments: attachments.map((a) => ({ name: uploads.get(a) ?? a })) })
      setTimeout(() => setStatus('working'), 300)
      setTimeout(() => {
        push({ id: id(), role: 'assistant', text: `收到：「${text.slice(0, 40)}」。這是 mock 的回覆。`, created_at: new Date().toISOString(), attachments: [] })
        replying = false
        setStatus('idle')
      }, 2600)
    },
    async upload(file) {
      await guard()
      if (file.size > SHARE_FILE_MAX) throw new ShareHttpError(413)
      const att = `att_${uploads.size + 1}`
      uploads.set(att, file.name)
      return { id: att, name: file.name }
    },
    async files() {
      await guard()
      return files.map(({ body: _b, ...f }) => f)
    },
    fileUrl(name) {
      const f = files.find((x) => x.name === name)
      return URL.createObjectURL(new Blob([f?.body ?? ''], { type: 'application/octet-stream' }))
    },
    subscribe(ev) {
      subs.push(ev)
      return () => {
        subs = subs.filter((s) => s !== ev)
      }
    },
  }
}
