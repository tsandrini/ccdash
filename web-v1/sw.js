// Service worker of the panel: keeps the app shell available offline
// (network first, cache as the fallback; the API, the stream and `/act`
// are never touched) and shows Web Push notifications with buttons that
// answer prompts through `/act/<key>/<action>`. Tapping the body opens the
// session's sheet; a `close` message takes an answered prompt down.

const CACHE = "ccdash-shell-v1";
const SHELL = [
  "./",
  "index.html",
  "app.js",
  "app.css",
  "icon.svg",
  "icon-192.png",
  "manifest.webmanifest",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(
          keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)),
        ),
      )
      .then(() => self.clients.claim()),
  );
});

const scope = new URL(self.registration.scope);

/** Shell files only: same origin, GET, not under the API. */
function isShell(request) {
  if (request.method !== "GET") return false;
  const url = new URL(request.url);
  if (url.origin !== scope.origin) return false;
  const path = url.pathname.slice(scope.pathname.length);
  if (path.startsWith("api/") || path.startsWith("act/") || path === "collect")
    return false;
  return request.mode === "navigate" || SHELL.includes(path);
}

self.addEventListener("fetch", (event) => {
  if (!isShell(event.request)) return;
  const key =
    event.request.mode === "navigate" ? new Request(scope.href) : event.request;
  event.respondWith(
    fetch(event.request)
      .then((response) => {
        if (response.ok) {
          const copy = response.clone();
          caches.open(CACHE).then((cache) => cache.put(key, copy));
        }
        return response;
      })
      .catch(async () => (await caches.match(key)) || Response.error()),
  );
});

// --- push -------------------------------------------------------------------

self.addEventListener("push", (event) => {
  let data = {};
  try {
    data = event.data?.json() ?? {};
  } catch {
    data = { title: "ccdash", body: event.data?.text() ?? "" };
  }
  if (data.type === "close") {
    event.waitUntil(closeTag(data.tag));
    return;
  }
  event.waitUntil(show(data));
});

async function closeTag(tag) {
  if (!tag) return;
  for (const n of await self.registration.getNotifications({ tag })) n.close();
}

/** Lays the hub's buttons out within what this platform shows. */
function actions(data) {
  const max = Notification.maxActions ?? 0;
  const buttons = (data.buttons ?? []).map((b) => ({
    action: b.action,
    title: b.label,
  }));
  if (!max) return [];
  if (data.more) {
    return [...buttons.slice(0, max - 1), { action: "open", title: "Answer" }];
  }
  return buttons.slice(0, max);
}

function show(data) {
  return self.registration.showNotification(data.title ?? "ccdash", {
    body: data.body ?? "",
    tag: data.tag,
    icon: "icon-192.png",
    badge: "icon-192.png",
    timestamp: data.at,
    requireInteraction: Boolean(data.urgent),
    actions: actions(data),
    data: { url: data.url, session: data.session, key: data.key },
  });
}

self.addEventListener("notificationclick", (event) => {
  const { url, session, key } = event.notification.data ?? {};
  event.notification.close();
  if (event.action && event.action !== "open" && key) {
    event.waitUntil(
      fetch(new URL(`act/${key}/${event.action}`, scope), { method: "POST" }),
    );
    return;
  }
  event.waitUntil(openSession(url, session));
});

/** Focuses an open panel and tells it which session, else opens one. */
async function openSession(url, session) {
  const windows = await self.clients.matchAll({
    type: "window",
    includeUncontrolled: true,
  });
  const client = windows.find((c) => new URL(c.url).origin === scope.origin);
  if (client) {
    if (session) client.postMessage({ type: "session", id: session });
    // NOTE: focus() rejects without a user gesture; the message got through anyway
    await client.focus().catch(() => {});
    return;
  }
  await self.clients.openWindow(url || scope.href);
}

// NOTE: the page re-sends the current subscription at every start, so the hub learns the new endpoint
self.addEventListener("pushsubscriptionchange", (event) => {
  const options = event.oldSubscription?.options;
  if (!options?.applicationServerKey) return;
  event.waitUntil(self.registration.pushManager.subscribe(options));
});
