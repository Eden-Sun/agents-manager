import { createElement, useState, type MouseEvent } from 'react'
import { ImageViewer, type Picture } from './ImageViewer'

/** Only collect already authorized, loaded thumbnails within this message/tray. */
export function useImageViewer(scope?: string | null) {
  const [gallery, setGallery] = useState<{ pictures: Picture[]; index: number; scope?: string | null } | null>(null)
  if (gallery && gallery.scope !== scope) setGallery(null)
  const openImage = (event: MouseEvent<HTMLElement>) => {
    const trigger = event.currentTarget
    const image = trigger.querySelector<HTMLImageElement>('img')
    if (!image?.src) return false
    event.preventDefault()
    event.stopPropagation()
    trigger.focus({ preventScroll: true })
    const group = trigger.closest('.bubble, .msg-attachments, .phone-outbox, [data-image-gallery]') ?? trigger
    const images = [...group.querySelectorAll<HTMLImageElement>('[data-viewable-image] img')]
    if (!images.includes(image)) images.push(image)
    setGallery({ scope, pictures: images.map((img) => ({ src: img.src, name: img.alt })), index: images.indexOf(image) })
    return true
  }
  return { openImage, viewer: gallery && gallery.scope === scope ? createElement(ImageViewer, { ...gallery, onClose: () => setGallery(null) }) : null }
}
