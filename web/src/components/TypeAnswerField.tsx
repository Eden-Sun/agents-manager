/** `Type something.` 展開的多行輸入。點選項本體只負責選取；打字在這裡，不送鍵。 */
export function TypeAnswerField({
  value,
  onChange,
  disabled,
}: {
  value: string
  onChange: (v: string) => void
  disabled?: boolean
}) {
  return (
    <textarea
      className="bc-type"
      rows={3}
      value={value}
      disabled={disabled}
      placeholder="打你的答案（可多行、可中文、可貼上）"
      aria-label="自訂答案"
      onChange={(e) => onChange(e.target.value)}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    />
  )
}
