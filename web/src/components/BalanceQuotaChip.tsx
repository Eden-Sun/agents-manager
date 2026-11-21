import { useState } from 'react'
import { putClaudeApiBalanceAnchor } from '../api/claudeApiBalance'
import { reloadClaudeApiBalance } from '../hooks/useClaudeApiBalance'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { anchorAge, balanceTooltipLines } from './balanceTooltip'
import { useClaudeBalanceView } from './balanceHooks'
import { Modal } from './Modal'
import { KindGlyph } from './KindTag'
import './balanceQuotaChip.css'
import { userTimeZone, zonedParts } from '../lib/userTime'

const hhmm = (iso: string | null | undefined): string => {
  const d = iso ? new Date(iso) : null
  if (!d || Number.isNaN(d.getTime())) return '--:--'
  const z = zonedParts(d, userTimeZone())
  return `${String(z.hour).padStart(2, '0')}:${String(z.minute).padStart(2, '0')}`
}

export function BalanceQuotaChip({ variant = 'chip' }: { variant?: 'chip' | 'row' } = {}) {
  const t = useT()
  const { balance, view } = useClaudeBalanceView()
  const apiBotId = useStore((s) => s.bots.find((b) => b.kind === 'claude-api')?.id ?? null)
  const selectBot = useStore((s) => s.selectBot)
  const notify = useStore((s) => s.notify)
  const [open, setOpen] = useState(false)
  const [usdText, setUsdText] = useState('')
  const [saving, setSaving] = useState(false)
  if (view.usd === undefined) return null
  const exact = view.source === 'console'
  const usd = view.usd
  const stale = !exact && usd < 0
  const text = stale ? t('需校正') : `$${usd.toFixed(1)}`
  const lines = balanceTooltipLines(balance, view, t)
  const stamp = exact ? balance?.console?.fetched_at : balance?.as_of
  const title = [
    stale
      ? t('餘額估算成負數：錨點已過舊（之後有儲值或贈送額度，官方查不到）。點一下用 Console 目前餘額校正')
      : exact
        ? t('Anthropic API 餘額（Console 實際值）')
        : t('Anthropic API 餘額（估算）'),
    ...lines,
    t('更新於 {time}', { time: hhmm(stamp) }),
    t('點一下：用 Console 目前餘額校正，或到 claude-api bot'),
  ]
    .filter(Boolean)
    .join('\n')
  const warn = stale || view.loggedOut || view.expiringSoon || (balance?.uncertain_usd ?? 0) > 0
  const cls = variant === 'row' ? `quota-pop-balance${warn ? ' warn' : ''}` : `quota-balance${exact ? ' exact' : ' estimate'}${warn ? ' warn' : ''}`
  const body =
    variant === 'row' ? (
      <>
        <KindGlyph kind="claude" label={t('Claude 餘額')} />
        <span className="quota-pop-balance-v">{text}</span>
      </>
    ) : (
      <>
        <KindGlyph kind="claude" label={t('Claude 餘額')} />
        <span className="quota-balance-v">{text}</span>
      </>
    )
  const usdVal = Number.parseFloat(usdText)
  const save = async () => {
    if (!Number.isFinite(usdVal) || usdVal < 0 || saving) return
    setSaving(true)
    try {
      await putClaudeApiBalanceAnchor(usdVal)
      await reloadClaudeApiBalance()
      notify('info', t('已用 Console 餘額 ${usd} 校正', { usd: usdVal.toFixed(2) }))
      setOpen(false)
      setUsdText('')
    } catch (err) {
      notify('error', err instanceof Error ? err.message : String(err))
    } finally {
      setSaving(false)
    }
  }
  return (
    <>
      <button type="button" className={cls} data-k={variant === 'row' ? 'quota-balance-row' : 'quota-balance'} title={title} onClick={() => setOpen(true)}>
        {body}
      </button>
      <Modal open={open} title={t('Anthropic API 餘額')} width={420} onClose={() => setOpen(false)}>
        <div className="quota-balance-dialog" data-k="quota-balance-dialog">
          {stale ? (
            <p className="hint warn" data-k="stale-note">
              {t('估算出來是負的（{usd}）：從錨點以來官方記到的花費已經超過錨點金額，表示期間有儲值或贈送額度。請用 Console 目前餘額校正。', { usd: `$${usd.toFixed(2)}` })}
            </p>
          ) : null}
          {lines.map((l) => (
            <p key={l} className="hint">{l}</p>
          ))}
          <label className="field">
            <span>{t('以 Console 目前餘額校正（美元）')}</span>
            <input
              type="number"
              step="0.01"
              min="0"
              autoFocus
              value={usdText}
              placeholder={t('例如 65.67')}
              onChange={(e) => setUsdText(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter') void save() }}
            />
          </label>
          <p className="hint" data-k="anchor-age">
            {balance?.anchor ? t('距上次校正：{age}', { age: anchorAge(balance.anchor.at, new Date(), t) }) : t('還沒校正過')}
          </p>
          <div className="form-actions">
            <button type="button" className="btn primary" data-k="anchor-save" disabled={!Number.isFinite(usdVal) || usdVal < 0 || saving} onClick={() => void save()}>
              {t('校正')}
            </button>
            {apiBotId ? (
              <button type="button" className="btn" data-k="goto-api-bot" onClick={() => { setOpen(false); selectBot(apiBotId) }}>
                {t('到 claude-api bot')}
              </button>
            ) : null}
          </div>
        </div>
      </Modal>
    </>
  )
}
