// Keeps the app's own files on the phone so it opens instantly, even with no signal.
// The page is fetched fresh when online (so updates show up right away); the saved copy is only used offline.
// Searches and streams are never cached.
const CACHE = 'rw-v2';   // bump when cached files change (keep the 'rw-' start: other apps share this web address)
const FILES = ['./', 'index.html', 'manifest.webmanifest', 'favicon.png', 'icon-192.png', 'icon-512.png'];

self.addEventListener('install', e => {
  e.waitUntil(caches.open(CACHE).then(c => c.addAll(FILES)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', e => {
  e.waitUntil(
    caches.keys()
      .then(keys => Promise.all(keys.filter(k => k.startsWith('rw-') && k !== CACHE).map(k => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', e => {
  const req = e.request;
  if (req.method !== 'GET' || new URL(req.url).origin !== location.origin) return;
  if (!new URL(req.url).pathname.startsWith(new URL('./', self.registration.scope).pathname)) return;
  if (req.mode === 'navigate') {
    e.respondWith(
      fetch(req, { cache: 'no-cache' })
        .then(res => { if (res.ok) { const copy = res.clone(); caches.open(CACHE).then(c => c.put('./', copy)); } return res; })
        .catch(() => caches.match('./'))
    );
    return;
  }
  e.respondWith(caches.match(req).then(hit => hit || fetch(req)));
});
