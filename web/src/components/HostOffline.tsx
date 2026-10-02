import { useState } from 'react'
import { useBotOfflineHost, useNow, useOfflineHosts } from '../hooks/useHostOffline'
import { downFor, type OfflineHost } from '../lib/hostOffline'
import { useStore } from '../store/store'
import './hostOffline.css'

/**
 * 遠端主機離線要一眼看得到（2026-10-02 使用者：「m4p offline，要更明顯一點」）：
 * 主畫面頂端的紅色警示條、聊天區頂端的橫幅、側欄整組反灰。判斷都在 `lib/hostOffline.ts`。
 */

function RetryButton({ host, className }: { host: string; className: string }) {
  const busy = useStore((s) => Boolean(s.busy[`host:${host}`]))
  const reconnectHost = useStore((s) => s.reconnectHost)
  return (
    <button type="button" className={className} disabled={busy} onClick={() => void reconnectHost(host)}>
      {busy ? '重連中…' : '立即重連'}
    </button>
  )
}

function HostRow({ h, now }: { h: OfflineHost; now: number }) {
  const [open, setOpen] = useState(false)
  const selectBot = useStore((s) => s.selectBot)
  const selectedBotId = useStore((s) => s.selectedBotId)
  const dur = downFor(h.since, now)
  const n = h.bots.length
  return (
    <div className="hob-host">
      <div className="hob-line">
        <span className="hob-icon" aria-hidden="true">
          ⚠
        </span>
        <span className="hob-title">
          主機 <b>{h.name}</b> 離線{dur ? <span className="hob-dur">{dur === '剛剛' ? '（剛剛斷線）' : `已 ${dur}`}</span> : null}
        </span>
        <button
          type="button"
          className="hob-toggle"
          aria-expanded={open}
          disabled={n === 0}
          title={h.error ? `原因：${h.error}` : undefined}
          onClick={() => setOpen((v) => !v)}
        >
          {n > 0 ? `影響 ${n} 顆 bot` : '沒有 bot'}
          {n > 0 ? <span aria-hidden="true">{open ? ' ▴' : ' ▾'}</span> : null}
        </button>
        <RetryButton host={h.name} className="hob-retry" />
      </div>
      {open ? (
        <div className="hob-detail">
          <p className="hob-note">
            {h.error ? <>原因：{h.error}。</> : null}
            daemon 會自動重試連線；這段期間這些 bot 收不到訊息，燈號顯示「主機離線」而不是最後一次的狀態。
          </p>
          <ul className="hob-bots">
            {h.bots.map((b) => (
              <li key={b.id}>
                <button
                  type="button"
                  className={`hob-bot${b.id === selectedBotId ? ' on' : ''}`}
                  onClick={() => {
                    selectBot(b.id)
                    setOpen(false)
                  }}
                >
                  <span className="hob-bot-name">{b.name}</span>
                  <span className="hob-bot-proj">{b.project}</span>
                </button>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  )
}

/** 主畫面最上面那條：每台離線主機一行（哪台、多久、影響幾顆），點開列出受影響的 bot。 */
export function HostOfflineBanner() {
  const down = useOfflineHosts()
  const now = useNow(down.length > 0)
  if (down.length === 0) return null
  return (
    <div className="host-offline-banner" role="alert">
      {down.map((h) => (
        <HostRow key={h.name} h={h} now={now} />
      ))}
    </div>
  )
}

/** 打開離線主機上的 bot：聊天區頂端講清楚送不出去、主機回來後會怎樣。 */
export function HostOfflineChatNotice({ botId }: { botId: string }) {
  const host = useBotOfflineHost(botId)
  const since = useStore((s) => (host ? (s.hosts.find((h) => h.name === host)?.disconnected_since ?? null) : null))
  const now = useNow(Boolean(host))
  if (!host) return null
  const dur = downFor(since, now)
  return (
    <div className="host-offline-chat" role="status">
      <div className="hoc-head">
        <span aria-hidden="true">⚠</span> 這顆 bot 在 <b>{host}</b> 上，主機離線{dur && dur !== '剛剛' ? ` ${dur}` : ''}：訊息現在送不出去
      </div>
      <p className="hoc-body">
        輸入框可以先打，字會留著。{host} 連回來後 daemon 會自動對帳，燈號恢復、離線期間 bot 在那台寫下的回覆會補進對話，再按送出即可。
      </p>
      <RetryButton host={host} className="hoc-retry" />
    </div>
  )
}

/** 側欄 bot 列的狀態字：主機離線時取代「閒置／執行中」，不留最後一次的狀態誤導人。 */
export function HostDownState() {
  return <span className="bot-state host-down">主機離線</span>
}
