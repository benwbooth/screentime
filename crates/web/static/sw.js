// Service worker: shows time-request notifications and handles their buttons.

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("push", (event) => {
  let data = {};
  try { data = event.data ? event.data.json() : {}; } catch (_) { data = { title: "screentime", body: event.data && event.data.text() }; }
  const actions = (data.actions || []).slice(0, 2).map((a, i) => ({ action: "a" + i, title: a.title }));
  event.waitUntil(self.registration.showNotification(data.title || "screentime", {
    body: data.body || "",
    icon: "/icon-192.png",
    badge: "/icon-192.png",
    // One notification per kid's request: a newer request replaces the older one, and still alerts.
    tag: data.tag || data.url || "screentime",
    renotify: true,
    requireInteraction: true,
    actions,
    data,
  }));
});

self.addEventListener("notificationclick", (event) => {
  const data = event.notification.data || {};
  event.notification.close();
  const index = event.action ? Number(event.action.slice(1)) : -1;
  const action = (data.actions || [])[index];
  if (action) {
    event.waitUntil(
      fetch(action.url, { method: "POST" })
        .then((r) => self.registration.showNotification(r.ok ? "Done: " + action.title : "Couldn't add the time", {
          body: r.ok ? "" : "Open screentime to try again.",
          icon: "/icon-192.png",
          tag: "screentime-result",
        }))
        .catch(() => self.clients.openWindow(data.url || "/"))
    );
    return;
  }
  event.waitUntil(self.clients.openWindow(data.url || "/"));
});
