import { useStore } from '../store/store'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'

export function ApiModelConflictDialog() {
  const t = useT()
  const conflict = useStore((s) => s.apiModelConflict)
  const resolve = useStore((s) => s.resolveApiModelConflict)
  const setConflict = useStore((s) => s.setApiModelConflict)
  const from = conflict?.checkpointModel ?? ''
  const to = conflict?.currentModel ?? ''
  return (
    <ConfirmDialog
      open={conflict !== null}
      title={t('模型換了，舊對話無法接續')}
      body={t('這段對話是用 {from} 產生的，bot 目前的模型是 {to}。可以換回 {from} 接續原對話，或清除對話以 {to} 重新開始。', { from, to })}
      confirmLabel={t('換回 {from}', { from })}
      cancelLabel={t('先不要')}
      secondaryLabel={t('清除對話')}
      onConfirm={() => void resolve('revert')}
      onSecondary={() => void resolve('clear')}
      onCancel={() => setConflict(null)}
    />
  )
}
