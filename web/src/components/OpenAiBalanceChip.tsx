import { useState } from 'react'
import { openAiBalanceView, putOpenAiApiBalanceAnchor } from '../api/openaiApiBalance'
import { reloadOpenAiApiBalance, useOpenAiApiBalance } from '../hooks/useOpenAiApiBalance'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { Modal } from './Modal'
import { KindGlyph } from './KindTag'
import { openAiConsoleTooltipLines } from './balanceTooltip'
import { useUserTimeZone } from '../lib/userTime'
import './balanceQuotaChip.css'

export function OpenAiBalanceChip({ variant = 'chip' }: { variant?: 'chip' | 'row' } = {}) {
  const t = useT()
  const balance = useOpenAiApiBalance()
  const notify = useStore((s) => s.notify)
  const [open, setOpen] = useState(false)
  const [usdText, setUsdText] = useState('')
  const [saving, setSaving] = useState(false)
  const tz = useUserTimeZone()

  if (!balance || !balance.enabled) return null

  const view = openAiBalanceView(balance, new Date(), tz)
  const exact = view.source === 'console'
  const configured = balance.configured || exact
  const usd = view.usd ?? 0
  const uncertain = exact ? 0 : balance.uncertain_usd ?? 0
  const consoleLines = openAiConsoleTooltipLines(balance, view, t)
  const title = exact
    ? [t('OpenAI API 餘額（Console 實際值）'), ...consoleLines, t('點一下：用 OpenAI 目前餘額校正')].join('\n')
    : configured
    ? [
        ...consoleLines,
        t('OpenAI API 餘額（估算）'),
        t('錨點：${usd}（{at}）', { usd: (balance.anchor?.usd ?? 0).toFixed(2), at: balance.anchor?.at ?? '' }),
        t('已確定花費：${spent}', { spent: (balance.spent_usd ?? 0).toFixed(2) }),
        uncertain > 0 ? t('另有結果不明的請求最多 ${unk}（以預留上限計）', { unk: uncertain.toFixed(2) }) : '',
        t('點一下：用 OpenAI 目前餘額校正'),
      ]
        .filter(Boolean)
        .join('\n')
    : [...consoleLines, t('OpenAI API 餘額：尚未設定。點一下輸入目前餘額當錨點')].join('\n')
  const warn = view.loggedOut || view.expiringSoon || (configured && (usd < 0 || uncertain > 0))
  const cls = variant === 'row' ? `quota-pop-balance${warn ? ' warn' : ''}` : `quota-balance${exact ? ' exact' : ' estimate'}${warn ? ' warn' : ''}`

  const usdVal = Number.parseFloat(usdText)
  const save = async () => {
    if (!Number.isFinite(usdVal) || usdVal < 0 || saving) return
    setSaving(true)
    try {
      await putOpenAiApiBalanceAnchor(usdVal)
      await reloadOpenAiApiBalance()
      notify('info', t('已用 OpenAI 餘額 ${usd} 校正', { usd: usdVal.toFixed(2) }))
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
      <button type="button" className={cls} data-k={variant === 'row' ? 'openai-balance-row' : 'openai-balance'} title={title} onClick={() => setOpen(true)}>
        <KindGlyph kind="codex" label={t('OpenAI API 餘額')} />
        <span className={variant === 'row' ? 'quota-pop-balance-v' : 'quota-balance-v'}>
          {configured ? `$${usd.toFixed(1)}` : t('設定 OpenAI 餘額')}
        </span>
      </button>
      <Modal open={open} title={t('OpenAI API 餘額')} width={420} onClose={() => setOpen(false)}>
        <div className="quota-balance-dialog" data-k="openai-balance-dialog">
          {consoleLines.map((l) => (
            <p key={l} className={`hint${view.loggedOut ? ' warn' : ''}`}>{l}</p>
          ))}
          {configured ? (
            <p className="hint">{t('目前餘額：{usd}', { usd: `$${usd.toFixed(2)}` })}</p>
          ) : (
            <p className="hint">{t('還沒設定餘額錨點。輸入 OpenAI 帳戶目前的可用餘額，之後以本機花費往下扣。')}</p>
          )}
          {exact ? null : <p className="hint">{t('OpenAI 沒有餘額查詢端點，儲值與贈送額度只能靠校正。')}</p>}
          <label className="field">
            <span>{t('以 OpenAI 目前餘額校正（美元）')}</span>
            <input
              type="number"
              step="0.01"
              min="0"
              autoFocus
              value={usdText}
              placeholder={t('例如 20.00')}
              onChange={(e) => setUsdText(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter') void save() }}
            />
          </label>
          <div className="form-actions">
            <button type="button" className="btn primary" data-k="openai-anchor-save" disabled={!Number.isFinite(usdVal) || usdVal < 0 || saving} onClick={() => void save()}>
              {t('校正')}
            </button>
          </div>
        </div>
      </Modal>
    </>
  )
}
