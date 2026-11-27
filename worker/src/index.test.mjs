import assert from "node:assert/strict";
import { test } from "node:test";

// The worker relies on Cloudflare's FixedLengthStream (takes the expected byte length); Node only
// ships TransformStream.
if (typeof globalThis.FixedLengthStream === "undefined") {
  globalThis.FixedLengthStream = class FixedLengthStream {
    constructor() {
      const stream = new TransformStream();
      this.readable = stream.readable;
      this.writable = stream.writable;
    }
  };
}

const { default: worker } = await import("./index.js");

const UPLOAD_KEY = "test-upload-key";
const SETTINGS_KEY = "_meta/settings.json";
const INVITES_KEY = "_meta/invites.json";
const HOUR = 3600;
const DAY = 24 * HOUR;

// --- in-memory R2 stand-in ----------------------------------------------------------------

function bytesToStream(bytes) {
  return new ReadableStream({
    start(controller) {
      controller.enqueue(bytes);
      controller.close();
    },
  });
}

async function consume(body) {
  if (body === null || body === undefined) return new Uint8Array(0);
  if (typeof body === "string") return new TextEncoder().encode(body);
  if (body instanceof Uint8Array) return body;
  if (typeof body.arrayBuffer === "function" && typeof body.getReader !== "function") {
    return new Uint8Array(await body.arrayBuffer());
  }
  const reader = body.getReader();
  const chunks = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    total += value.byteLength;
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

class MockBucket {
  constructor() {
    this.objects = new Map();
    this.deletedBatches = [];
    this.listCalls = [];
  }

  async put(key, value, options = {}) {
    const onlyIf = options.onlyIf;
    if (onlyIf?.etagDoesNotMatch === "*" && this.objects.has(key)) {
      return null;
    }
    const bytes = await consume(value);
    const previous = this.objects.get(key);
    this.objects.set(key, {
      bytes,
      uploaded: new Date(),
      httpMetadata: options.httpMetadata || {},
      customMetadata: options.customMetadata || {},
      etag: previous ? previous.etag : `"etag-${this.objects.size + 1}-${bytes.byteLength}"`,
    });
    return this.summary(key);
  }

  async get(key /*, options */) {
    const stored = this.objects.get(key);
    if (!stored) return null;
    return this.body(key, stored);
  }

  async head(key) {
    return this.objects.has(key) ? this.summary(key) : null;
  }

  async list(options = {}) {
    const limit = options.limit ?? 1000;
    const include = new Set(options.include || []);
    this.listCalls.push({ limit, cursor: options.cursor, include: [...include] });
    const keys = [...this.objects.keys()].sort();
    const start = options.cursor ? keys.indexOf(options.cursor) : 0;
    const slice = keys.slice(start, start + limit);
    const truncated = start + limit < keys.length;
    return {
      // Like R2: customMetadata is only returned when `include` asks for it.
      objects: slice.map((key) => {
        const summary = this.summary(key);
        if (!include.has("customMetadata")) delete summary.customMetadata;
        return summary;
      }),
      truncated,
      cursor: truncated ? keys[start + limit] : undefined,
      delimitedPrefixes: [],
    };
  }

  async delete(keys) {
    const list = Array.isArray(keys) ? keys : [keys];
    this.deletedBatches.push(list);
    for (const key of list) this.objects.delete(key);
  }

  // Test helper: backdate an object so TTL behaviour can be exercised without waiting.
  setAgeSeconds(key, seconds) {
    const stored = this.objects.get(key);
    assert.ok(stored, `no object ${key}`);
    stored.uploaded = new Date(Date.now() - seconds * 1000);
  }

  rawJson(key) {
    const stored = this.objects.get(key);
    return stored ? new TextDecoder().decode(stored.bytes) : null;
  }

  objectCustomMetadata(key) {
    return this.objects.get(key)?.customMetadata || {};
  }

  get rawText() {
    return this.objects.has(SETTINGS_KEY)
      ? new TextDecoder().decode(this.objects.get(SETTINGS_KEY).bytes)
      : null;
  }

  summary(key) {
    const stored = this.objects.get(key);
    return {
      key,
      size: stored.bytes.byteLength,
      etag: stored.etag,
      httpEtag: stored.etag,
      uploaded: stored.uploaded,
      customMetadata: stored.customMetadata,
      httpMetadata: stored.httpMetadata,
    };
  }

  body(key, stored) {
    return {
      ...this.summary(key),
      range: undefined,
      body: bytesToStream(stored.bytes),
      async text() {
        return new TextDecoder().decode(stored.bytes);
      },
      async arrayBuffer() {
        return stored.bytes.buffer.slice(
          stored.bytes.byteOffset,
          stored.bytes.byteOffset + stored.bytes.byteLength,
        );
      },
      writeHttpMetadata(headers) {
        if (stored.httpMetadata.contentType) {
          headers.set("Content-Type", stored.httpMetadata.contentType);
        }
        if (stored.httpMetadata.cacheControl) {
          headers.set("Cache-Control", stored.httpMetadata.cacheControl);
        }
      },
    };
  }
}

// --- helpers -------------------------------------------------------------------------------

function makeEnv(bucket) {
  return { CLIPS_BUCKET: bucket, UPLOAD_KEY };
}

function authHeaders(extra = {}) {
  return { Authorization: `Bearer ${UPLOAD_KEY}`, ...extra };
}

function mp4Body(label = "clip") {
  const payload = new TextEncoder().encode(`payload:${label}`);
  const out = new Uint8Array(12 + payload.byteLength);
  out.set([0x00, 0x00, 0x00, 0x18, 0x66, 0x74, 0x79, 0x70, 0x69, 0x73, 0x6f, 0x6d], 0);
  out.set(payload, 12);
  return out;
}

function pngBody() {
  const payload = new TextEncoder().encode("thumb");
  const out = new Uint8Array(8 + payload.byteLength);
  out.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], 0);
  out.set(payload, 8);
  return out;
}

async function upload(bucket, slug, bytes) {
  const res = await worker.fetch(
    new Request(`https://clips.test/upload/${slug}.mp4`, {
      method: "PUT",
      headers: authHeaders({
        "Content-Length": String(bytes.byteLength),
        "X-Clip-Title": slug,
      }),
      body: bytes,
    }),
    makeEnv(bucket),
    {},
  );
  assert.equal(res.status, 200, await res.text());
  return slug;
}

async function get(bucket, path, init) {
  return worker.fetch(new Request(`https://clips.test${path}`, init), makeEnv(bucket), {});
}

async function setTtl(bucket, ttlSeconds, method = "PUT") {
  return worker.fetch(
    new Request("https://clips.test/admin/settings", {
      method,
      headers: authHeaders({ "Content-Type": "application/json" }),
      body: JSON.stringify({ ttlSeconds }),
    }),
    makeEnv(bucket),
    {},
  );
}

async function stats(bucket) {
  const res = await worker.fetch(
    new Request("https://clips.test/admin/stats", { headers: authHeaders() }),
    makeEnv(bucket),
    {},
  );
  return { status: res.status, body: await res.json() };
}

function bearer(token) {
  return { Authorization: `Bearer ${token}` };
}

async function adminFetch(bucket, path, { method = "GET", token = UPLOAD_KEY, body } = {}) {
  const headers = bearer(token);
  if (body !== undefined) headers["Content-Type"] = "application/json";
  return worker.fetch(
    new Request(`https://clips.test${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    }),
    makeEnv(bucket),
    {},
  );
}

async function createInvite(bucket, body = {}, token = UPLOAD_KEY) {
  const res = await adminFetch(bucket, "/admin/invites", { method: "POST", token, body });
  const jsonBody = await res.json();
  assert.equal(res.status, 200, JSON.stringify(jsonBody));
  return jsonBody;
}

async function uploadAs(bucket, token, slug, bytes, ext = "mp4") {
  return worker.fetch(
    new Request(`https://clips.test/upload/${slug}.${ext}`, {
      method: "PUT",
      headers: {
        ...bearer(token),
        "Content-Length": String(bytes.byteLength),
        ...(ext === "mp4" ? { "X-Clip-Title": slug } : {}),
      },
      body: bytes,
    }),
    makeEnv(bucket),
    {},
  );
}

async function statsAs(bucket, token = UPLOAD_KEY) {
  const res = await adminFetch(bucket, "/admin/stats", { token });
  return { status: res.status, body: await res.json() };
}

async function setTtlAs(bucket, token, ttlSeconds) {
  return adminFetch(bucket, "/admin/settings", { method: "PUT", token, body: { ttlSeconds } });
}

function keys(bucket) {
  return [...bucket.objects.keys()].sort();
}

// --- tests ---------------------------------------------------------------------------------

test("upload then public fetch serves the clip, page and thumb", async () => {
  const bucket = new MockBucket();
  const video = mp4Body("one");
  await upload(bucket, "one", video);

  const videoRes = await get(bucket, "/one.mp4");
  assert.equal(videoRes.status, 200);
  assert.equal(videoRes.headers.get("Content-Type"), "video/mp4");
  assert.deepEqual(new Uint8Array(await videoRes.arrayBuffer()), video);

  const pageRes = await get(bucket, "/one");
  assert.equal(pageRes.status, 200);
  const html = await pageRes.text();
  assert.match(html, /<video /);
  assert.match(html, /https:\/\/clips\.test\/one\.mp4/);

  const thumbUpload = await worker.fetch(
    new Request("https://clips.test/upload/one.png", {
      method: "PUT",
      headers: authHeaders({ "Content-Length": String(pngBody().byteLength) }),
      body: pngBody(),
    }),
    makeEnv(bucket),
    {},
  );
  assert.equal(thumbUpload.status, 200);
  const thumbRes = await get(bucket, "/one.png");
  assert.equal(thumbRes.status, 200);
  assert.equal(thumbRes.headers.get("Content-Type"), "image/png");
});

test("upload requires the right key", async () => {
  const bucket = new MockBucket();
  const bytes = mp4Body();
  const res = await worker.fetch(
    new Request("https://clips.test/upload/nope.mp4", {
      method: "PUT",
      headers: { "Content-Length": String(bytes.byteLength) },
      body: bytes,
    }),
    makeEnv(bucket),
    {},
  );
  assert.equal(res.status, 401);
});

test("/admin/* requires auth", async () => {
  const bucket = new MockBucket();
  for (const [path, method] of [
    ["/admin/stats", "GET"],
    ["/admin/settings", "PUT"],
    ["/admin/clips", "DELETE"],
  ]) {
    const res = await worker.fetch(
      new Request(`https://clips.test${path}`, { method }),
      makeEnv(bucket),
      {},
    );
    assert.equal(res.status, 401, `${path} should be 401 without auth`);
  }
});

test("/admin/stats counts bytes, clips and expired objects", async () => {
  const bucket = new MockBucket();
  await upload(bucket, "old", mp4Body("old"));
  await upload(bucket, "new", mp4Body("new"));
  await worker.fetch(
    new Request("https://clips.test/upload/new.png", {
      method: "PUT",
      headers: authHeaders({ "Content-Length": String(pngBody().byteLength) }),
      body: pngBody(),
    }),
    makeEnv(bucket),
    {},
  );
  bucket.setAgeSeconds("old.mp4", 4 * DAY);

  const { status, body } = await stats(bucket);
  assert.equal(status, 200);
  assert.equal(body.ok, true);
  assert.equal(body.clipCount, 2);
  assert.equal(body.objectCount, 3);
  assert.equal(body.expiredCount, 1);
  assert.equal(body.ttlSeconds, 3 * DAY);
  assert.equal(body.freeTierBytes, 10_000_000_000);
  assert.equal(
    body.usedBytes,
    bucket.summary("old.mp4").size + bucket.summary("new.mp4").size + bucket.summary("new.png").size,
  );
});

test("/admin/settings validates the TTL", async () => {
  const bucket = new MockBucket();
  for (const bad of [30, -1, 1.5, "600", 365 * 86400 + 1, null, undefined]) {
    const res = await setTtl(bucket, bad);
    assert.equal(res.status, 400, `ttl ${JSON.stringify(bad)} should be rejected`);
    assert.ok((await res.json()).error);
  }
  const badJson = await worker.fetch(
    new Request("https://clips.test/admin/settings", {
      method: "PUT",
      headers: authHeaders(),
      body: "not json",
    }),
    makeEnv(bucket),
    {},
  );
  assert.equal(badJson.status, 400);

  const ok = await setTtl(bucket, 5 * DAY);
  assert.equal(ok.status, 200);
  assert.deepEqual(await ok.json(), { ok: true, ttlSeconds: 5 * DAY });
  assert.equal(JSON.parse(bucket.rawText).ttlSeconds, 5 * DAY);

  // POST is accepted too.
  const posted = await setTtl(bucket, 0, "POST");
  assert.equal(posted.status, 200);
  assert.deepEqual(await posted.json(), { ok: true, ttlSeconds: 0 });

  const wrongMethod = await worker.fetch(
    new Request("https://clips.test/admin/settings", { method: "GET", headers: authHeaders() }),
    makeEnv(bucket),
    {},
  );
  assert.equal(wrongMethod.status, 405);

  const unknown = await worker.fetch(
    new Request("https://clips.test/admin/whatever", { headers: authHeaders() }),
    makeEnv(bucket),
    {},
  );
  assert.equal(unknown.status, 404);
});

test("TTL changes apply retroactively to clips already stored", async () => {
  const bucket = new MockBucket();
  await upload(bucket, "aged", mp4Body("aged"));
  bucket.setAgeSeconds("aged.mp4", 2 * DAY);

  // Default 3-day TTL: a 2-day-old clip is still live.
  assert.equal((await get(bucket, "/aged.mp4")).status, 200);
  assert.equal((await get(bucket, "/aged")).status, 200);

  // Shorten to 1 day -> expired immediately, with no rewrite of the object.
  assert.equal((await setTtl(bucket, 1 * DAY)).status, 200);
  assert.equal((await get(bucket, "/aged.mp4")).status, 404);
  assert.equal((await get(bucket, "/aged")).status, 404);

  // Lengthen back to 5 days -> restored.
  assert.equal((await setTtl(bucket, 5 * DAY)).status, 200);
  assert.equal((await get(bucket, "/aged.mp4")).status, 200);
  assert.equal((await get(bucket, "/aged")).status, 200);

  // 0 means never expire.
  assert.equal((await setTtl(bucket, 0)).status, 200);
  bucket.setAgeSeconds("aged.mp4", 10 * 365 * DAY);
  assert.equal((await get(bucket, "/aged.mp4")).status, 200);
});

test("Cache-Control is capped by the remaining TTL", async () => {
  const bucket = new MockBucket();
  await upload(bucket, "capped", mp4Body("capped"));
  bucket.setAgeSeconds("capped.mp4", 2 * DAY);

  // 3-day default leaves ~1 day: media caps at 1h, the page at 5m.
  let res = await get(bucket, "/capped.mp4");
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=3600");
  res = await get(bucket, "/capped");
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=300");

  // Short TTL: the media cap drops below the 1h ceiling to the remaining seconds.
  await setTtl(bucket, 2 * DAY + 90);
  res = await get(bucket, "/capped.mp4");
  const mediaMaxAge = Number(/max-age=(\d+)/.exec(res.headers.get("Cache-Control"))[1]);
  assert.ok(mediaMaxAge >= 1 && mediaMaxAge <= 90, `media max-age ${mediaMaxAge}`);
  assert.doesNotMatch(res.headers.get("Cache-Control"), /immutable/);
  res = await get(bucket, "/capped");
  const pageMaxAge = Number(/max-age=(\d+)/.exec(res.headers.get("Cache-Control"))[1]);
  assert.ok(pageMaxAge >= 1 && pageMaxAge <= 90, `page max-age ${pageMaxAge}`);

  // Never-expire keeps the plain lifetimes and no immutable directive.
  await setTtl(bucket, 0);
  res = await get(bucket, "/capped.mp4");
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=3600");
  res = await get(bucket, "/capped");
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=300");
});

test("scheduled() deletes only expired objects and keeps settings", async () => {
  const bucket = new MockBucket();
  await upload(bucket, "stale", mp4Body("stale"));
  await upload(bucket, "fresh", mp4Body("fresh"));
  await setTtl(bucket, 2 * DAY);
  bucket.setAgeSeconds("stale.mp4", 3 * DAY);

  await worker.scheduled({ cron: "0 * * * *" }, makeEnv(bucket), {});

  assert.equal(bucket.objects.has("stale.mp4"), false);
  assert.equal(bucket.objects.has("fresh.mp4"), true);
  assert.equal(bucket.objects.has(SETTINGS_KEY), true);
  assert.equal(JSON.parse(bucket.rawText).ttlSeconds, 2 * DAY);

  // "Never" skips the bucket listing entirely.
  await setTtl(bucket, 0);
  await worker.scheduled({ cron: "0 * * * *" }, makeEnv(bucket), {});
  assert.equal(bucket.objects.has("fresh.mp4"), true);
});

test("DELETE /admin/clips removes every clip and asset but keeps settings", async () => {
  const bucket = new MockBucket();
  await upload(bucket, "a", mp4Body("a"));
  await upload(bucket, "b", mp4Body("b"));
  for (const slug of ["a", "b"]) {
    await worker.fetch(
      new Request(`https://clips.test/upload/${slug}.png`, {
        method: "PUT",
        headers: authHeaders({ "Content-Length": String(pngBody().byteLength) }),
        body: pngBody(),
      }),
      makeEnv(bucket),
      {},
    );
  }
  await setTtl(bucket, 3 * DAY);

  const res = await worker.fetch(
    new Request("https://clips.test/admin/clips", { method: "DELETE", headers: authHeaders() }),
    makeEnv(bucket),
    {},
  );
  assert.equal(res.status, 200);
  assert.deepEqual(await res.json(), {
    ok: true,
    scope: "mine",
    deletedObjects: 4,
    deletedClips: 2,
  });
  assert.deepEqual([...bucket.objects.keys()], [SETTINGS_KEY]);

  const { body } = await stats(bucket);
  assert.equal(body.objectCount, 0);
  assert.equal(body.clipCount, 0);
  assert.equal(body.usedBytes, 0);
});

test("the settings object is not publicly routable", async () => {
  const bucket = new MockBucket();
  await setTtl(bucket, 3 * DAY);

  for (const path of ["/_meta/settings.json", "/_meta", "/settings.json"]) {
    const res = await get(bucket, path);
    assert.equal(res.status, 404, `${path} must 404`);
  }
});

test("anonymous admin subpaths still 404/405 behind auth", async () => {
  const bucket = new MockBucket();
  const res = await worker.fetch(
    new Request("https://clips.test/admin/stats", {
      method: "POST",
      headers: authHeaders(),
    }),
    makeEnv(bucket),
    {},
  );
  assert.equal(res.status, 405);
  assert.equal(res.headers.get("Allow"), "GET");
});

// --- multi-user: invites, ownership, scoped deletes ------------------------------------------

test("owner invites a friend whose uploads are tagged and counted per owner", async () => {
  const bucket = new MockBucket();
  await upload(bucket, "owner-mine", mp4Body("owner-mine"));

  const invite = await createInvite(bucket, { label: "Ada" });
  assert.match(invite.id, /^f_[0-9a-f]{8}$/);
  assert.equal(invite.label, "Ada");
  assert.equal(invite.ttlSeconds, 3 * DAY);
  assert.equal(invite.quotaBytes, 0);
  assert.equal(typeof invite.token, "string");
  assert.ok(invite.token.length >= 30);

  const stored = JSON.parse(bucket.rawJson(INVITES_KEY));
  assert.equal(stored.friends.length, 1);
  assert.equal(stored.friends[0].tokenHash.length, 64);
  assert.notEqual(stored.friends[0].tokenHash, invite.token);
  assert.equal(bucket.rawJson(INVITES_KEY).includes(invite.token), false);

  const friendVideo = mp4Body("friend");
  assert.equal((await uploadAs(bucket, invite.token, "friend-clip", friendVideo)).status, 200);
  assert.equal((await uploadAs(bucket, invite.token, "friend-clip", pngBody(), "png")).status, 200);

  // Both the video and the thumbnail carry the uploader tag.
  assert.equal(bucket.objectCustomMetadata("friend-clip.mp4").owner, invite.id);
  assert.equal(bucket.objectCustomMetadata("friend-clip.png").owner, invite.id);
  assert.equal(bucket.objectCustomMetadata("owner-mine.mp4").owner, "owner");

  const friend = (await statsAs(bucket, invite.token)).body;
  assert.equal(friend.role, "friend");
  assert.equal(friend.label, "Ada");
  assert.equal(friend.usedBytes, friendVideo.byteLength + pngBody().byteLength);
  assert.equal(friend.clipCount, 1);
  assert.equal(friend.objectCount, 2);
  assert.equal(friend.quotaBytes, 0);
  assert.equal(friend.ttlSeconds, 3 * DAY);
  assert.deepEqual(friend.friends, []);
  assert.equal(friend.bucketClipCount, 2);
  assert.equal(friend.bucketObjectCount, 3);
  assert.equal(friend.bucketUsedBytes, friend.usedBytes + bucket.summary("owner-mine.mp4").size);

  const owner = (await statsAs(bucket)).body;
  assert.equal(owner.role, "owner");
  assert.equal(owner.label, null);
  assert.equal(owner.clipCount, 1);
  assert.equal(owner.objectCount, 1);
  assert.equal(owner.bucketClipCount, 2);
  assert.equal(owner.bucketObjectCount, 3);
  assert.deepEqual(owner.friends, [
    {
      id: invite.id,
      label: "Ada",
      ttlSeconds: 3 * DAY,
      quotaBytes: 0,
      createdAt: invite.createdAt,
      usedBytes: friendVideo.byteLength + pngBody().byteLength,
      clipCount: 1,
      objectCount: 2,
    },
  ]);
  assert.equal(JSON.stringify(owner.friends).includes(stored.friends[0].tokenHash), false);

  // Every listing that drives ownership decisions must ask R2 for customMetadata.
  assert.ok(bucket.listCalls.length > 0);
  assert.ok(bucket.listCalls.every((call) => call.include.includes("customMetadata")));
});

test("invite creation validates its input and is owner-only", async () => {
  const bucket = new MockBucket();
  await setTtl(bucket, 5 * DAY);

  const defaults = await adminFetch(bucket, "/admin/invites", { method: "POST", body: {} });
  assert.equal(defaults.status, 200);
  const created = await defaults.json();
  assert.equal(created.label, "Friend");
  assert.equal(created.ttlSeconds, 5 * DAY);
  assert.equal(created.quotaBytes, 0);

  const explicit = await createInvite(bucket, {
    label: "  Bob  ",
    ttlSeconds: 0,
    quotaBytes: 1024,
  });
  assert.equal(explicit.label, "Bob");
  assert.equal(explicit.ttlSeconds, 0);
  assert.equal(explicit.quotaBytes, 1024);

  for (const bad of [
    { label: "x".repeat(41) },
    { label: 5 },
    { ttlSeconds: 30 },
    { ttlSeconds: "600" },
    { quotaBytes: -1 },
    { quotaBytes: 1.5 },
    { quotaBytes: "10" },
  ]) {
    const res = await adminFetch(bucket, "/admin/invites", { method: "POST", body: bad });
    assert.equal(res.status, 400, `${JSON.stringify(bad)} must be rejected`);
  }

  // A friend token can neither create nor revoke invites.
  const created2 = await adminFetch(bucket, "/admin/invites", {
    method: "POST",
    token: explicit.token,
    body: { label: "Sneaky" },
  });
  assert.equal(created2.status, 403);
  const revoked2 = await adminFetch(bucket, `/admin/invites/${created.id}`, {
    method: "DELETE",
    token: explicit.token,
  });
  assert.equal(revoked2.status, 403);
  assert.equal(JSON.parse(bucket.rawJson(INVITES_KEY)).friends.length, 2);

  const listRes = await adminFetch(bucket, "/admin/invites");
  assert.equal(listRes.status, 405);
  assert.equal(listRes.headers.get("Allow"), "POST");
});

test("per-owner TTL expires only that owner's clips, in both directions", async () => {
  const bucket = new MockBucket();
  const invite = await createInvite(bucket, { label: "Ada", ttlSeconds: 3 * DAY });

  await upload(bucket, "owner-two-day", mp4Body("owner-two-day"));
  await uploadAs(bucket, invite.token, "friend-two-day", mp4Body("friend-two-day"));
  await upload(bucket, "owner-thumb", mp4Body("owner-thumb"));
  await uploadAs(bucket, invite.token, "friend-thumb", mp4Body("friend-thumb"));
  await uploadAs(bucket, UPLOAD_KEY, "owner-thumb", pngBody(), "png");
  await uploadAs(bucket, invite.token, "friend-thumb", pngBody(), "png");
  for (const key of [
    "owner-two-day.mp4",
    "friend-two-day.mp4",
    "owner-thumb.png",
    "friend-thumb.png",
  ]) {
    bucket.setAgeSeconds(key, 2 * DAY);
  }

  // Default policy for both: a 2-day-old clip is still inside a 3-day TTL.
  assert.equal((await get(bucket, "/owner-two-day.mp4")).status, 200);
  assert.equal((await get(bucket, "/friend-two-day.mp4")).status, 200);

  // The friend shortens only their own TTL to 1 day.
  const res = await setTtlAs(bucket, invite.token, 1 * DAY);
  assert.equal(res.status, 200);
  assert.deepEqual(await res.json(), { ok: true, ttlSeconds: 1 * DAY });
  assert.equal(JSON.parse(bucket.rawJson(INVITES_KEY)).friends[0].ttlSeconds, 1 * DAY);
  assert.equal(bucket.objects.has(SETTINGS_KEY), false); // the owner policy is untouched

  assert.equal((await get(bucket, "/friend-two-day.mp4")).status, 404);
  assert.equal((await get(bucket, "/friend-two-day")).status, 404);
  assert.equal((await get(bucket, "/friend-thumb.png")).status, 404);
  assert.equal((await get(bucket, "/owner-two-day.mp4")).status, 200);
  assert.equal((await get(bucket, "/owner-two-day")).status, 200);
  assert.equal((await get(bucket, "/owner-thumb.png")).status, 200);

  // ... and the other way round: the friend lives 5 days while the owner drops to 1.
  assert.equal((await setTtlAs(bucket, invite.token, 5 * DAY)).status, 200);
  assert.equal((await setTtl(bucket, 1 * DAY)).status, 200);
  assert.equal((await get(bucket, "/friend-two-day.mp4")).status, 200);
  assert.equal((await get(bucket, "/friend-thumb.png")).status, 200);
  assert.equal((await get(bucket, "/owner-two-day.mp4")).status, 404);
  assert.equal((await get(bucket, "/owner-two-day")).status, 404);
  assert.equal((await get(bucket, "/owner-thumb.png")).status, 404);

  // A revoked friend id falls back to the owner policy instead of living forever.
  await setTtl(bucket, 0);
  assert.equal((await setTtlAs(bucket, invite.token, 0)).status, 200);
  bucket.setAgeSeconds("friend-two-day.mp4", 4 * DAY);
  assert.equal((await get(bucket, "/friend-two-day.mp4")).status, 200);
  assert.equal(
    (await adminFetch(bucket, `/admin/invites/${invite.id}`, { method: "DELETE" })).status,
    200,
  );
  await setTtl(bucket, 3 * DAY);
  assert.equal((await get(bucket, "/friend-two-day.mp4")).status, 404);
});

test("friends can only delete their own clips", async () => {
  const bucket = new MockBucket();
  const invite = await createInvite(bucket, { label: "Ada" });
  await upload(bucket, "owner-clip", mp4Body("owner-clip"));
  await uploadAs(bucket, invite.token, "friend-clip", mp4Body("friend-clip"));
  await uploadAs(bucket, invite.token, "friend-clip", pngBody(), "png");

  const mine = await adminFetch(bucket, "/admin/clips", {
    method: "DELETE",
    token: invite.token,
  });
  assert.equal(mine.status, 200);
  assert.deepEqual(await mine.json(), {
    ok: true,
    scope: "mine",
    deletedObjects: 2,
    deletedClips: 1,
  });
  assert.deepEqual(keys(bucket), [INVITES_KEY, "owner-clip.mp4"].sort());

  const all = await adminFetch(bucket, "/admin/clips?scope=all", {
    method: "DELETE",
    token: invite.token,
  });
  assert.equal(all.status, 403);
  assert.deepEqual(await all.json(), {
    error: "Only the owner can delete everyone's clips",
  });
  assert.deepEqual(keys(bucket), [INVITES_KEY, "owner-clip.mp4"].sort());

  const invalid = await adminFetch(bucket, "/admin/clips?scope=everyone", {
    method: "DELETE",
    token: invite.token,
  });
  assert.equal(invalid.status, 400);
  assert.deepEqual(keys(bucket), [INVITES_KEY, "owner-clip.mp4"].sort());
});

test("owner deletes mine vs all and never the meta objects", async () => {
  const bucket = new MockBucket();
  const invite = await createInvite(bucket, { label: "Ada" });
  await upload(bucket, "owner-clip", mp4Body("owner-clip"));
  await uploadAs(bucket, invite.token, "friend-clip", mp4Body("friend-clip"));
  await setTtl(bucket, 2 * DAY);

  const mine = await adminFetch(bucket, "/admin/clips?scope=mine", { method: "DELETE" });
  assert.equal(mine.status, 200);
  assert.deepEqual(await mine.json(), {
    ok: true,
    scope: "mine",
    deletedObjects: 1,
    deletedClips: 1,
  });
  assert.deepEqual(keys(bucket), [INVITES_KEY, SETTINGS_KEY, "friend-clip.mp4"].sort());

  await upload(bucket, "owner-two", mp4Body("owner-two"));
  const all = await adminFetch(bucket, "/admin/clips?scope=all", { method: "DELETE" });
  assert.equal(all.status, 200);
  assert.deepEqual(await all.json(), {
    ok: true,
    scope: "all",
    deletedObjects: 2,
    deletedClips: 2,
  });
  assert.deepEqual(keys(bucket), [INVITES_KEY, SETTINGS_KEY].sort());
  assert.equal(JSON.parse(bucket.rawJson(SETTINGS_KEY)).ttlSeconds, 2 * DAY);
  assert.equal(JSON.parse(bucket.rawJson(INVITES_KEY)).friends.length, 1);
});

test("friend quota is enforced before anything is written", async () => {
  const bucket = new MockBucket();
  const invite = await createInvite(bucket, { label: "Ada", quotaBytes: 30 });
  assert.equal(mp4Body("a").byteLength, 21);

  assert.equal((await uploadAs(bucket, invite.token, "fit", mp4Body("a"))).status, 200);

  const rejected = await uploadAs(bucket, invite.token, "over", mp4Body("b"));
  assert.equal(rejected.status, 413);
  assert.deepEqual(await rejected.json(), { error: "Friend storage limit reached" });
  assert.equal(bucket.objects.has("over.mp4"), false);

  // The owner is not bound by a friend's quota, and other friends have their own.
  assert.equal((await uploadAs(bucket, UPLOAD_KEY, "owner-ok", mp4Body("c"))).status, 200);
  const other = await createInvite(bucket, { label: "Bob", quotaBytes: 30 });
  assert.equal((await uploadAs(bucket, other.token, "bob-ok", mp4Body("d"))).status, 200);

  // quotaBytes 0 means unlimited.
  const unlimited = await createInvite(bucket, { label: "Cid", quotaBytes: 0 });
  for (const slug of ["u1", "u2", "u3"]) {
    assert.equal((await uploadAs(bucket, unlimited.token, slug, mp4Body(slug))).status, 200);
  }

  // A thumb counts against the quota exactly like a video.
  const constrained = await createInvite(bucket, { label: "Dee", quotaBytes: 21 });
  assert.equal((await uploadAs(bucket, constrained.token, "dee-1", mp4Body("e"))).status, 200);
  assert.equal((await uploadAs(bucket, constrained.token, "dee-2", pngBody(), "png")).status, 413);
});

test("revoking an invite kills the token and can wipe its clips", async () => {
  const bucket = new MockBucket();
  const invite = await createInvite(bucket, { label: "Ada" });
  await upload(bucket, "keep-me", mp4Body("keep-me"));
  await uploadAs(bucket, invite.token, "gone-1", mp4Body("gone-1"));
  await uploadAs(bucket, invite.token, "gone-2", mp4Body("gone-2"));
  await uploadAs(bucket, invite.token, "gone-2", pngBody(), "png");

  const res = await adminFetch(bucket, `/admin/invites/${invite.id}?deleteClips=1`, {
    method: "DELETE",
  });
  assert.equal(res.status, 200);
  assert.deepEqual(await res.json(), {
    ok: true,
    id: invite.id,
    deletedObjects: 3,
    deletedClips: 2,
  });

  // The token is dead immediately on every authenticated route.
  assert.equal((await statsAs(bucket, invite.token)).status, 401);
  assert.equal((await uploadAs(bucket, invite.token, "nope", mp4Body("n"))).status, 401);
  assert.equal((await setTtlAs(bucket, invite.token, 1 * DAY)).status, 401);
  assert.deepEqual(JSON.parse(bucket.rawJson(INVITES_KEY)).friends, []);
  assert.deepEqual(keys(bucket), [INVITES_KEY, "keep-me.mp4"].sort());

  // Revoking without deleteClips keeps the files (now under the owner policy).
  const second = await createInvite(bucket, { label: "Bob" });
  await uploadAs(bucket, second.token, "bob-clip", mp4Body("bob"));
  const plain = await adminFetch(bucket, `/admin/invites/${second.id}`, { method: "DELETE" });
  assert.equal(plain.status, 200);
  assert.deepEqual(await plain.json(), {
    ok: true,
    id: second.id,
    deletedObjects: 0,
    deletedClips: 0,
  });
  assert.equal(bucket.objects.has("bob-clip.mp4"), true);

  const missing = await adminFetch(bucket, "/admin/invites/f_deadbeef", { method: "DELETE" });
  assert.equal(missing.status, 404);
});

test("scheduled() applies each owner's TTL independently", async () => {
  const bucket = new MockBucket();
  const shortFriend = await createInvite(bucket, { label: "Short", ttlSeconds: 1 * DAY });
  const longFriend = await createInvite(bucket, { label: "Long", ttlSeconds: 5 * DAY });
  await setTtl(bucket, 2 * DAY);

  await upload(bucket, "owner-stale", mp4Body("owner-stale"));
  await upload(bucket, "owner-fresh", mp4Body("owner-fresh"));
  await uploadAs(bucket, shortFriend.token, "short-stale", mp4Body("short-stale"));
  await uploadAs(bucket, longFriend.token, "long-live", mp4Body("long-live"));

  bucket.setAgeSeconds("owner-stale.mp4", 3 * DAY);
  bucket.setAgeSeconds("short-stale.mp4", 3 * DAY);
  bucket.setAgeSeconds("long-live.mp4", 3 * DAY);

  await worker.scheduled({ cron: "0 * * * *" }, makeEnv(bucket), {});

  assert.equal(bucket.objects.has("owner-stale.mp4"), false);
  assert.equal(bucket.objects.has("short-stale.mp4"), false);
  assert.equal(bucket.objects.has("owner-fresh.mp4"), true);
  assert.equal(bucket.objects.has("long-live.mp4"), true);
  assert.equal(bucket.objects.has(SETTINGS_KEY), true);
  assert.equal(bucket.objects.has(INVITES_KEY), true);

  // Every owner at "never" skips the listing entirely.
  await setTtl(bucket, 0);
  assert.equal((await setTtlAs(bucket, shortFriend.token, 0)).status, 200);
  assert.equal((await setTtlAs(bucket, longFriend.token, 0)).status, 200);
  const listCallsBefore = bucket.listCalls.length;
  bucket.setAgeSeconds("long-live.mp4", 10 * 365 * DAY);
  await worker.scheduled({ cron: "0 * * * *" }, makeEnv(bucket), {});
  assert.equal(bucket.listCalls.length, listCallsBefore);
  assert.equal(bucket.objects.has("long-live.mp4"), true);
});

test("the invites object is not publicly routable", async () => {
  const bucket = new MockBucket();
  await createInvite(bucket, { label: "Ada" });

  for (const path of ["/_meta/invites.json", "/_meta/settings.json", "/_meta", "/invites.json"]) {
    const res = await get(bucket, path);
    assert.equal(res.status, 404, `${path} must 404`);
  }
});
