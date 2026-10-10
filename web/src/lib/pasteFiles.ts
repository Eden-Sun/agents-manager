/** 這次貼上要當成檔案收下的那些；回空陣列時呼叫端應照常讓文字貼上。 */
export function pastedFiles(dt: { files?: ArrayLike<File> | null; types?: ArrayLike<string> | null; getData(type: string): string } | null | undefined): File[] {
  const files = Array.from(dt?.files ?? [])
  if (files.length === 0) return []
  // Office 類程式複製文字時可能附上內容截圖；這種組合應視為文字貼上。
  const types = Array.from(dt?.types ?? [])
  const officeText = types.includes('text/rtf') && dt!.getData('text/plain').trim() !== '' && files.every((file) => file.type.startsWith('image/'))
  return officeText ? [] : files
}
