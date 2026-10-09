// Corral runs the web app without a service worker: the app is useless offline (no router, no model), and
// the worker's cache served the old page on reload, hiding both the "Corral is off" page and the Corral title.
// Browsers that installed the app's worker fetch this file when they check it for updates, so this
// replacement takes over once, deletes the app's caches, removes itself, and reloads open tabs from the
// server. It deletes only Cache Storage; chats live in IndexedDB and are left alone.
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => {
  event.waitUntil((async () => {
    for (const name of await caches.keys()) {
      await caches.delete(name);
    }
    await self.registration.unregister();
    for (const tab of await self.clients.matchAll({ type: "window" })) {
      tab.navigate(tab.url);
    }
  })());
});
