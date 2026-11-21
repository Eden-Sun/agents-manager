self.addEventListener('install', () => self.skipWaiting())
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()))

self.addEventListener('push', (event) => {
  let data = {}
  try {
    data = event.data ? event.data.json() : {}
  } catch {
    data = { body: event.data ? event.data.text() : '' }
  }
  if (data.close && data.tag) {
    event.waitUntil(
      (async () => {
        await self.registration.showNotification(' ', { tag: data.tag, silent: true, icon: '/icon-192.png' })
        const list = await self.registration.getNotifications({ tag: data.tag })
        list.forEach((n) => n.close())
      })(),
    )
    return
  }
  const title = data.title || 'AG Man'
  event.waitUntil(
    self.registration.showNotification(title, {
      body: data.body || '',
      tag: data.tag || undefined,
      icon: '/icon-192.png',
      badge: '/icon-192.png',
      data: { url: data.url || '/' },
    }),
  )
})

self.addEventListener('notificationclick', (event) => {
  event.notification.close()
  const url = (event.notification.data && event.notification.data.url) || '/'
  event.waitUntil(
    (async () => {
      const all = await self.clients.matchAll({ type: 'window', includeUncontrolled: true })
      for (const c of all) {
        if ('focus' in c) {
          await c.focus()
          c.postMessage({ type: 'agm-open-url', url })
          return
        }
      }
      await self.clients.openWindow(url)
    })(),
  )
})
