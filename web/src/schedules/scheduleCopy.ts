/** 名稱欄上限 80 個 UTF-16 code units；保留完整字元並替副本後綴留空間。 */
export function copyScheduleName(name: string): string {
  const suffix = '（副本）'
  const chars = Array.from(name.trim())
  while (chars.join('').length > 80 - suffix.length) chars.pop()
  return `${chars.join('')}${suffix}`
}
