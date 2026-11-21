import type { FormEvent, ReactNode } from 'react'

export interface CapEditorProps {
  formClassName: string
  formTestId?: string
  labelClassName?: string
  inputClassName?: string
  btnClassName: string
  labelText: string
  inputId: string
  value: string
  saving: boolean
  showClear: boolean
  saveText: string
  clearText: string
  cancelText: string
  onChange: (value: string) => void
  onSubmit: (e: FormEvent) => void
  onClear: () => void
  onCancel: () => void
  footer?: ReactNode
}

export function CapEditor(p: CapEditorProps) {
  return (
    <form className={p.formClassName} data-testid={p.formTestId} onSubmit={p.onSubmit}>
      <label className={p.labelClassName} htmlFor={p.inputId}>{p.labelText}</label>
      <input
        id={p.inputId}
        className={p.inputClassName}
        type="number"
        step="0.01"
        min="0"
        autoFocus
        value={p.value}
        disabled={p.saving}
        onChange={(e) => p.onChange(e.target.value)}
        placeholder="0.00"
      />
      <button className={p.btnClassName} type="submit" disabled={p.saving}>
        {p.saveText}
      </button>
      {p.showClear ? (
        <button className={`${p.btnClassName} clear`} type="button" disabled={p.saving} onClick={p.onClear}>
          {p.clearText}
        </button>
      ) : null}
      <button className={`${p.btnClassName} cancel`} type="button" disabled={p.saving} onClick={p.onCancel}>
        {p.cancelText}
      </button>
      {p.footer}
    </form>
  )
}
