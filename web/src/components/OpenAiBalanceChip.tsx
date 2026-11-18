import { useState } from 'react'
import { putOpenAiApiBalanceAnchor } from '../api/openaiApiBalance'
import { reloadOpenAiApiBalance, useOpenAiApiBalance } from '../hooks/useOpenAiApiBalance'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { Modal } from './Modal'
import './balanceQuotaChip.css'

export function OpenAiBalanceChip() {
  const t = useT()
  const balance = useOpenAiApiBalance()
  const notify = useStore((s) => s.notify)
  const [open, setOpen] = useState(false)
  const [usdText, setUsdText] = useState('')
  const [saving, setSaving] = useState(false)

  if (!balance || !balance.configured) return null
  
  const usd = balance.remaining_usd ?? 0
  const text = `$${usd.toFixed(2)}`
  
  const save = async () => {
    const usdVal = Number.parseFloat(usdText)
    if (!Number.isFinite(usdVal) || usdVal < 0 || saving) return
    setSaving(true)
    try {
      await putOpenAiApiBalanceAnchor(usdVal)
      await reloadOpenAiApiBalance()
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
      <button type="button" className="quota-balance estimate" title={t('OpenAI API 餘額')} onClick={() => setOpen(true)}>
        <span className="quota-balance-k">OpenAI</span>
        <span className="quota-balance-v">{text}</span>
      </button>
      <Modal open={open} title={t('OpenAI API 餘額')} width={420} onClose={() => setOpen(false)}>
        <div className="quota-balance-dialog">
          <p className="hint">{t('目前餘額：{usd}', { usd: `$${usd.toFixed(2)}` })}</p>
          <label className="field">
            <span>{t('以 Console 目前餘額校正（美元）')}</span>
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
            <button type="button" className="btn primary" disabled={saving} onClick={() => void save()}>
              {t('校正')}
            </button>
          </div>
        </div>
      </Modal>
    </>
  )
}
