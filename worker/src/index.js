const SLUG_RE = /^[A-Za-z0-9_-]{1,80}$/;
const MAX_VIDEO_BYTES = 500 * 1024 * 1024;
const MAX_THUMB_BYTES = 2 * 1024 * 1024;
const COMPONENT_EMBED_MAX_BYTES = 3000;
const TEXT_ENCODER = new TextEncoder();
const SECURITY_HEADERS = {
  "X-Content-Type-Options": "nosniff",
  "Referrer-Policy": "no-referrer",
  "X-Frame-Options": "DENY",
};
const PNG_SIGNATURE = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
const FTYP_MIN_BYTES = 12;
const BIDI_CONTROL_RE = /[\u061C\u200E\u200F\u202A-\u202E\u2066-\u2069]/;

// Reserved object keys for worker-side state. Not publicly routable: every public path is a
// single segment that must match SLUG_RE, which rejects both "/" and ".".
const SETTINGS_KEY = "_meta/settings.json";
const INVITES_KEY = "_meta/invites.json";
const META_PREFIX = "_meta/";
// Legacy objects carry no owner metadata and belong to the worker's own upload key.
const OWNER_ID = "owner";
const FRIEND_LABEL_MAX = 40;
const DEFAULT_FRIEND_LABEL = "Friend";
const TOKEN_BYTES = 24;
const DEFAULT_TTL_SECONDS = 3 * 24 * 3600;
const MAX_TTL_SECONDS = 365 * 86400;
const MIN_TTL_SECONDS = 60;
// Standard Cloudflare free-tier R2 storage allowance, used only for reporting.
const FREE_TIER_BYTES = 10_000_000_000;
const LIST_LIMIT = 1000;
const MEDIA_CACHE_MAX_AGE = 3600;
const PAGE_CACHE_MAX_AGE = 300;
const NEVER_MEDIA_CACHE = `public, max-age=${MEDIA_CACHE_MAX_AGE}`;
const NEVER_PAGE_CACHE = `public, max-age=${PAGE_CACHE_MAX_AGE}`;

// Guarded so unit tests can run this module in runtimes without the Workers cache API.
const edgeCache = typeof caches !== "undefined" ? caches.default : null;

class UploadError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    const path = url.pathname;

    if (request.method === "GET" && path === "/") {
      return new Response("clip share worker", {
        status: 200,
        headers: withSecurityHeaders(
          new Headers({ "Content-Type": "text/plain; charset=utf-8" }),
        ),
      });
    }

    if (path.startsWith("/upload/")) {
      if (request.method !== "PUT" && request.method !== "POST") {
        return json({ error: "Method not allowed" }, 405, { Allow: "PUT, POST" });
      }
      return handleUpload(request, env, url, path.slice("/upload/".length));
    }

    if (path === "/admin" || path.startsWith("/admin/")) {
      return handleAdmin(request, env, ctx, url);
    }

    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("Method not allowed", {
        status: 405,
        headers: withSecurityHeaders(new Headers({ Allow: "GET, HEAD" })),
      });
    }

    const name = path.slice(1);
    if (!name || name.includes("/")) {
      return notFound();
    }

    if (name.endsWith(".mp4")) {
      const slug = name.slice(0, -4);
      if (!SLUG_RE.test(slug)) {
        return notFound();
      }
      return serveCached(request, ctx, () => handleVideo(request, env, `${slug}.mp4`));
    }

    if (name.endsWith(".png")) {
      const slug = name.slice(0, -4);
      if (!SLUG_RE.test(slug)) {
        return notFound();
      }
      return serveCached(request, ctx, () => handleThumb(request, env, `${slug}.png`));
    }

    if (!SLUG_RE.test(name)) {
      return notFound();
    }
    return serveCached(request, ctx, () => handlePage(request, env, url, name));
  },

  // Hourly Cron trigger: deletes objects whose age exceeds the current TTL setting.
  async scheduled(event, env, ctx) {
    await deleteExpiredObjects(env);
  },
};

// Serves a clip response through the edge cache. Only plain GETs without a Range header are
// cacheable; misses are (re)stored in the background, but only when the response is actually
// cacheable. Expiring responses (a short max-age from a shortened TTL) must never be pinned at the
// edge, otherwise they would outlive the TTL they were capped for.
async function serveCached(request, ctx, render) {
  if (request.method !== "GET" || !edgeCache) {
    return render();
  }
  if (request.headers.get("Range")) {
    return render();
  }

  const cached = await edgeCache.match(request);
  if (cached) {
    return cached;
  }

  const response = await render();
  if (response.status === 200 && isCacheableResponse(response)) {
    ctx?.waitUntil(edgeCache.put(request, response.clone()));
  }
  return response;
}

// A response is only stored at the edge when its Cache-Control opts into a positive lifetime.
function isCacheableResponse(response) {
  const cacheControl = response.headers.get("Cache-Control") || "";
  if (!cacheControl) return false;
  if (/\b(no-store|no-cache|private)\b/i.test(cacheControl)) return false;
  const maxAge = /\bmax-age=(\d+)\b/i.exec(cacheControl);
  return maxAge ? Number(maxAge[1]) > 0 : false;
}

async function handleUpload(request, env, url, rawName) {
  const { auth, response: authFailure } = await resolveAuth(request, env);
  if (authFailure) {
    return authFailure;
  }

  let decodedName;
  try {
    decodedName = decodeURIComponent(rawName);
  } catch {
    return json({ error: "Invalid clip name" }, 400);
  }

  const isThumb = decodedName.endsWith(".png");
  const isVideo = decodedName.endsWith(".mp4");
  if (!isVideo && !isThumb) {
    return json({ error: "File must end with .mp4 or .png" }, 400);
  }

  const slug = decodedName.slice(0, -4);
  if (!SLUG_RE.test(slug)) {
    return json({ error: "Slug must match [A-Za-z0-9_-]{1,80}" }, 400);
  }

  // The declared length is the only bound we enforce up front: a missing or bogus value must
  // never let an unbounded body through.
  const declared = parseContentLength(request.headers.get("Content-Length"));
  if (!declared) {
    return request.headers.has("Content-Length")
      ? json({ error: "Content-Length must be a positive integer" }, 400)
      : json({ error: "Content-Length is required" }, 411);
  }
  const minBytes = isThumb ? PNG_SIGNATURE.length : FTYP_MIN_BYTES;
  if (declared < minBytes) {
    return json({ error: "Unsupported media type" }, 415);
  }
  const maxBytes = isThumb ? MAX_THUMB_BYTES : MAX_VIDEO_BYTES;
  if (declared > maxBytes) {
    return json({ error: "Payload too large" }, 413);
  }
  if (auth.role === "friend" && auth.friend.quotaBytes > 0) {
    let usedBytes = 0;
    for (const obj of await listAllObjects(env)) {
      if (isMetaKey(obj.key)) continue;
      if (objectOwner(obj) === auth.ownerId) usedBytes += Number(obj.size) || 0;
    }
    if (usedBytes + declared > auth.friend.quotaBytes) {
      return json({ error: "Friend storage limit reached" }, 413);
    }
  }
  if (!request.body) {
    return json({ error: "Empty request body" }, 400);
  }

  const contentType = isThumb ? "image/png" : "video/mp4";
  const putOptions = {
    httpMetadata: {
      contentType,
      // Short and revalidatable: the served lifetime is capped per request by the current TTL.
      cacheControl: NEVER_MEDIA_CACHE,
    },
    // Every object records its uploader: per-owner TTL, quota and scoped deletes all read this.
    customMetadata: { owner: auth.ownerId },
    // Never silently replace an existing object.
    onlyIf: { etagDoesNotMatch: "*" },
  };
  if (isVideo) {
    const customMetadata = putOptions.customMetadata;
    const title = sanitizeTitle(tryDecode(request.headers.get("X-Clip-Title")));
    if (title) customMetadata.title = title;
    const width = parseDimension(request.headers.get("X-Clip-Width"));
    const height = parseDimension(request.headers.get("X-Clip-Height"));
    if (width && height) {
      customMetadata.width = String(width);
      customMetadata.height = String(height);
    }
    if (request.headers.get("X-Clip-Has-Thumb") === "1") {
      customMetadata.hasThumb = "1";
    }
  }

  const key = isThumb ? `${slug}.png` : `${slug}.mp4`;
  const { readable, writable } = new FixedLengthStream(declared);
  let putError = null;
  const putPromise = env.CLIPS_BUCKET.put(key, readable, putOptions).catch((err) => {
    putError = err;
    return undefined;
  });

  let bodyError = null;
  try {
    await streamUpload(request.body, writable, isThumb ? "png" : "mp4", declared);
  } catch (err) {
    bodyError = err instanceof UploadError ? err : new UploadError(400, "Upload body was abandoned");
  }

  const stored = await putPromise;
  if (stored === null) {
    return json({ error: "A clip with this name already exists" }, 409);
  }
  if (bodyError) {
    return json({ error: bodyError.message }, bodyError.status || 400);
  }
  if (putError || !stored) {
    return json({ error: "Failed to store upload" }, 500);
  }

  if (isThumb) {
    return json({
      ok: true,
      id: slug,
      thumbUrl: `${url.origin}/${slug}.png`,
    });
  }

  return json({
    ok: true,
    id: slug,
    url: `${url.origin}/${slug}`,
    videoUrl: `${url.origin}/${slug}.mp4`,
  });
}

// Streams the request body into a `FixedLengthStream`, validating the container magic as soon as
// the leading bytes arrive. Nothing is buffered beyond the signature prefix, so Worker RAM stays
// flat for multi-hundred-MB uploads.
async function streamUpload(body, writable, kind, contentLength) {
  const reader = body.getReader();
  const writer = writable.getWriter();
  const needed = kind === "png" ? PNG_SIGNATURE.length : FTYP_MIN_BYTES;
  const prefix = new Uint8Array(needed);
  let prefixLen = 0;
  let magicOk = false;
  let seen = 0;

  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (!value || value.byteLength === 0) continue;

      seen += value.byteLength;
      if (seen > contentLength) {
        throw new UploadError(400, "Body is larger than Content-Length");
      }

      if (!magicOk) {
        const take = Math.min(needed - prefixLen, value.byteLength);
        prefix.set(value.subarray(0, take), prefixLen);
        prefixLen += take;
        if (prefixLen >= needed) {
          if (!hasExpectedMagic(prefix, kind)) {
            throw new UploadError(415, "Unsupported media type");
          }
          magicOk = true;
        }
      }

      await writer.write(value);
    }

    if (seen !== contentLength) {
      throw new UploadError(400, "Body is smaller than Content-Length");
    }
    if (!magicOk && !hasExpectedMagic(prefix.subarray(0, prefixLen), kind)) {
      throw new UploadError(415, "Unsupported media type");
    }
    await writer.close();
  } catch (err) {
    await writer.abort(err).catch(() => {});
    await reader.cancel().catch(() => {});
    throw err;
  }
}

function hasExpectedMagic(prefix, kind) {
  if (kind === "png") {
    if (prefix.byteLength < PNG_SIGNATURE.length) return false;
    for (let i = 0; i < PNG_SIGNATURE.length; i++) {
      if (prefix[i] !== PNG_SIGNATURE[i]) return false;
    }
    return true;
  }
  // ISO-BMFF: a 4-byte box size followed by the `ftyp` box type.
  if (prefix.byteLength < FTYP_MIN_BYTES) return false;
  return prefix[4] === 0x66 && prefix[5] === 0x74 && prefix[6] === 0x79 && prefix[7] === 0x70;
}

async function handleVideo(request, env, key) {
  const rangeHeader = request.headers.get("Range");

  if (request.method === "HEAD") {
    const [settings, invites, head] = await Promise.all([
      readSettings(env),
      readInvites(env),
      env.CLIPS_BUCKET.head(key),
    ]);
    if (!head) {
      return notFound();
    }
    const ttlSeconds = ttlForOwner(objectOwner(head), settings, invites);
    const remaining = remainingTtlSeconds(head, ttlSeconds);
    if (remaining <= 0) {
      return notFound();
    }
    const cacheControl = mediaCacheControl(ttlSeconds, remaining);
    const notModified = notModifiedResponse(request, head.httpEtag, cacheControl);
    if (notModified) {
      return notModified;
    }
    const headers = videoHeaders(head, key, cacheControl);
    if (rangeHeader) {
      const parsed = parseRange(rangeHeader, head.size);
      if (!parsed) {
        headers.set("Content-Range", `bytes */${head.size}`);
        return new Response(null, { status: 416, headers });
      }
      const length = parsed.end - parsed.start + 1;
      headers.set("Content-Range", `bytes ${parsed.start}-${parsed.end}/${head.size}`);
      headers.set("Content-Length", String(length));
      return new Response(null, { status: 206, headers });
    }
    headers.set("Content-Length", String(head.size));
    return new Response(null, { status: 200, headers });
  }

  // Single R2 roundtrip: let the bucket resolve the range instead of HEADing first. Settings and
  // invites are read in parallel because either can retroactively shorten the TTL.
  const [settings, invites, fetched] = await Promise.all([
    readSettings(env),
    readInvites(env),
    env.CLIPS_BUCKET.get(key, rangeHeader ? { range: request.headers } : undefined).catch(() => null),
  ]);
  const obj = fetched;
  if (!obj) {
    if (!rangeHeader) {
      return notFound();
    }
    const head = await env.CLIPS_BUCKET.head(key);
    if (!head) {
      return notFound();
    }
    const headTtl = ttlForOwner(objectOwner(head), settings, invites);
    const headRemaining = remainingTtlSeconds(head, headTtl);
    if (headRemaining <= 0) {
      return notFound();
    }
    const headers = videoHeaders(head, key, mediaCacheControl(headTtl, headRemaining));
    headers.set("Content-Range", `bytes */${head.size}`);
    return new Response(null, { status: 416, headers });
  }

  const ttlSeconds = ttlForOwner(objectOwner(obj), settings, invites);
  const remaining = remainingTtlSeconds(obj, ttlSeconds);
  if (remaining <= 0) {
    await cancelBody(obj);
    return notFound();
  }
  const cacheControl = mediaCacheControl(ttlSeconds, remaining);

  const notModified = notModifiedResponse(request, obj.httpEtag, cacheControl);
  if (notModified) {
    await cancelBody(obj);
    return notModified;
  }

  if (rangeHeader) {
    const parsed = parseRange(rangeHeader, obj.size);
    if (!parsed) {
      await cancelBody(obj);
      const headers = videoHeaders(obj, key, cacheControl);
      headers.set("Content-Range", `bytes */${obj.size}`);
      return new Response(null, { status: 416, headers });
    }
    if (obj.range && typeof obj.range.offset === "number") {
      const offset = obj.range.offset;
      const length =
        typeof obj.range.length === "number" ? obj.range.length : obj.size - offset;
      const headers = videoHeaders(obj, key, cacheControl);
      headers.set("Content-Range", `bytes ${offset}-${offset + length - 1}/${obj.size}`);
      headers.set("Content-Length", String(length));
      return new Response(obj.body, { status: 206, headers });
    }
    // The bucket ignored the range and handed back the whole object.
  }

  const headers = videoHeaders(obj, key, cacheControl);
  headers.set("Content-Length", String(obj.size));
  return new Response(obj.body, { status: 200, headers });
}

async function handleThumb(request, env, key) {
  if (request.method === "HEAD") {
    const [settings, invites, head] = await Promise.all([
      readSettings(env),
      readInvites(env),
      env.CLIPS_BUCKET.head(key),
    ]);
    if (!head) {
      return notFound();
    }
    const ttlSeconds = ttlForOwner(objectOwner(head), settings, invites);
    const remaining = remainingTtlSeconds(head, ttlSeconds);
    if (remaining <= 0) {
      return notFound();
    }
    const cacheControl = mediaCacheControl(ttlSeconds, remaining);
    const notModified = notModifiedResponse(request, head.httpEtag, cacheControl);
    if (notModified) {
      return notModified;
    }
    const headers = thumbHeaders(head, cacheControl);
    headers.set("Content-Length", String(head.size));
    return new Response(null, { status: 200, headers });
  }

  const [settings, invites, obj] = await Promise.all([
    readSettings(env),
    readInvites(env),
    env.CLIPS_BUCKET.get(key),
  ]);
  if (!obj) {
    return notFound();
  }
  const ttlSeconds = ttlForOwner(objectOwner(obj), settings, invites);
  const remaining = remainingTtlSeconds(obj, ttlSeconds);
  if (remaining <= 0) {
    await cancelBody(obj);
    return notFound();
  }
  const cacheControl = mediaCacheControl(ttlSeconds, remaining);
  const notModified = notModifiedResponse(request, obj.httpEtag, cacheControl);
  if (notModified) {
    await cancelBody(obj);
    return notModified;
  }
  const headers = thumbHeaders(obj, cacheControl);
  headers.set("Content-Length", String(obj.size));
  return new Response(obj.body, { status: 200, headers });
}

async function cancelBody(obj) {
  const body = obj?.body;
  if (!body || typeof body.cancel !== "function") return;
  try {
    await body.cancel();
  } catch {
    // Already cancelled or the stream never started; nothing to release.
  }
}

function videoHeaders(obj, key, cacheControl = NEVER_MEDIA_CACHE) {
  const headers = new Headers();
  obj.writeHttpMetadata(headers);
  headers.set("Content-Type", "video/mp4");
  headers.set("Accept-Ranges", "bytes");
  headers.set("ETag", obj.httpEtag);
  headers.set("Cache-Control", cacheControl);
  headers.set("Content-Disposition", `inline; filename="${key}"`);
  return withSecurityHeaders(headers);
}

function thumbHeaders(obj, cacheControl = NEVER_MEDIA_CACHE) {
  const headers = new Headers();
  obj.writeHttpMetadata(headers);
  headers.set("Content-Type", "image/png");
  headers.set("ETag", obj.httpEtag);
  headers.set("Cache-Control", cacheControl);
  return withSecurityHeaders(headers);
}

function notModifiedResponse(request, etag, cacheControl = NEVER_MEDIA_CACHE) {
  if (!etagMatches(request.headers.get("If-None-Match"), etag)) {
    return null;
  }
  const headers = new Headers();
  headers.set("ETag", etag);
  headers.set("Cache-Control", cacheControl);
  return new Response(null, { status: 304, headers: withSecurityHeaders(headers) });
}

function etagMatches(ifNoneMatch, etag) {
  if (!ifNoneMatch || !etag) return false;
  if (ifNoneMatch.trim() === "*") return true;
  return ifNoneMatch
    .split(",")
    .some((candidate) => candidate.trim().replace(/^W\//, "") === etag);
}

async function handlePage(request, env, url, slug) {
  const [settings, invites, head] = await Promise.all([
    readSettings(env),
    readInvites(env),
    env.CLIPS_BUCKET.head(`${slug}.mp4`),
  ]);
  if (!head) {
    return new Response("Clip not found", {
      status: 404,
      headers: withSecurityHeaders(
        new Headers({ "Content-Type": "text/plain; charset=utf-8" }),
      ),
    });
  }
  const ttlSeconds = ttlForOwner(objectOwner(head), settings, invites);
  const remaining = remainingTtlSeconds(head, ttlSeconds);
  if (remaining <= 0) {
    return notFound();
  }

  const headers = new Headers({
    "Content-Type": "text/html; charset=utf-8",
    "Cache-Control": pageCacheControl(ttlSeconds, remaining),
  });
  withSecurityHeaders(headers);
  if (request.method === "HEAD") {
    return new Response(null, { status: 200, headers });
  }

  const meta = head.customMetadata || {};
  const title = sanitizeTitle(meta.title) || slug;
  const width = parseDimension(meta.width);
  const height = parseDimension(meta.height);
  const hasThumb = meta.hasThumb === "1";

  const pageUrl = `${url.origin}/${slug}`;
  const videoUrl = `${url.origin}/${slug}.mp4`;
  const thumbUrl = hasThumb ? `${url.origin}/${slug}.png` : null;

  const escTitle = escapeHtml(title);
  const escPageUrl = escapeHtml(pageUrl);
  const escVideoUrl = escapeHtml(videoUrl);
  const componentEmbedTag = buildComponentEmbedTag(title, pageUrl, videoUrl);

  const ogDimTags =
    width && height
      ? `  <meta property="og:video:width" content="${width}" />\n  <meta property="og:video:height" content="${height}" />\n`
      : "";
  const ogThumbTags = thumbUrl
    ? `  <meta property="og:image" content="${escapeHtml(thumbUrl)}" />\n${
        width && height
          ? `  <meta property="og:image:width" content="${width}" />\n  <meta property="og:image:height" content="${height}" />\n`
          : ""
      }`
    : "";

  const html = `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>${escTitle} — clip</title>
  <meta property="og:type" content="video.other" />
  <meta property="og:title" content="${escTitle}" />
  <meta property="og:url" content="${escPageUrl}" />
  <meta property="og:video" content="${escVideoUrl}" />
  <meta property="og:video:secure_url" content="${escVideoUrl}" />
  <meta property="og:video:type" content="video/mp4" />
${ogDimTags}${ogThumbTags}${componentEmbedTag}</head>
<body style="margin:0;min-height:100vh;background:#111B24;color:#F2F8FA;font-family:system-ui,-apple-system,Segoe UI,sans-serif;display:flex;flex-direction:column;align-items:center;justify-content:center;padding:20px;box-sizing:border-box;">
  <main style="width:100%;max-width:960px;display:flex;flex-direction:column;gap:12px;">
    <video controls playsinline preload="metadata"${thumbUrl ? ` poster="${escapeHtml(thumbUrl)}"` : ""} src="${escVideoUrl}" style="width:100%;max-height:82vh;border-radius:6px;background:#080E14;border:0;"></video>
    <div style="display:flex;align-items:center;justify-content:space-between;gap:12px;font-size:14px;color:#9CB6C5;">
      <span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">${escTitle}</span>
      <a href="${escVideoUrl}" download="${escapeHtml(slug)}.mp4" style="color:#45D6CC;text-decoration:none;font-weight:600;white-space:nowrap;">Download MP4</a>
    </div>
  </main>
</body>
</html>`;

  return new Response(html, { status: 200, headers });
}

function extractAuthKey(request) {
  const auth = request.headers.get("Authorization") || "";
  if (auth.startsWith("Bearer ")) {
    return auth.slice(7).trim();
  }
  const alt = request.headers.get("X-Upload-Key");
  return alt ? alt.trim() : "";
}

// Compares fixed-length SHA-256 digests so the check leaks neither the key length nor content.
async function timingSafeEqual(a, b) {
  const [aBuf, bBuf] = await Promise.all([
    crypto.subtle.digest("SHA-256", TEXT_ENCODER.encode(String(a))),
    crypto.subtle.digest("SHA-256", TEXT_ENCODER.encode(String(b))),
  ]);
  if (typeof crypto.subtle.timingSafeEqual === "function") {
    return crypto.subtle.timingSafeEqual(aBuf, bBuf);
  }
  // Fallback for runtimes without the Workers extension (e.g. plain Node unit tests).
  const x = new Uint8Array(aBuf);
  const y = new Uint8Array(bBuf);
  let diff = x.length ^ y.length;
  for (let i = 0; i < x.length; i++) {
    diff |= x[i] ^ y[i];
  }
  return diff === 0;
}

// Resolves the caller from either the worker's own upload key or a friend invite token. Returns
// `{ auth }` on success or `{ response }` with the error to send. Every invite hash is compared on
// every attempt (no early exit) so a miss costs the same whatever the matching prefix is.
async function resolveAuth(request, env) {
  const expectedKey = typeof env.UPLOAD_KEY === "string" ? env.UPLOAD_KEY : "";
  if (!expectedKey) {
    return { response: json({ error: "Worker UPLOAD_KEY is not configured" }, 503) };
  }
  const providedKey = extractAuthKey(request);
  if (!providedKey) {
    return { response: json({ error: "Unauthorized" }, 401) };
  }
  if (await timingSafeEqual(providedKey, expectedKey)) {
    return { auth: { role: "owner", ownerId: OWNER_ID, friend: null } };
  }

  const { friends } = await readInvites(env);
  if (friends.length > 0) {
    const providedHash = await sha256Hex(providedKey);
    let matched = null;
    for (const friend of friends) {
      const same = await timingSafeEqual(providedHash, friend.tokenHash);
      if (same) matched = matched || friend;
    }
    if (matched) {
      return { auth: { role: "friend", ownerId: matched.id, friend: matched } };
    }
  }
  return { response: json({ error: "Unauthorized" }, 401) };
}

async function sha256Hex(value) {
  const digest = await crypto.subtle.digest("SHA-256", TEXT_ENCODER.encode(String(value)));
  let hex = "";
  for (const byte of new Uint8Array(digest)) {
    hex += byte.toString(16).padStart(2, "0");
  }
  return hex;
}

// --- Ownership ---------------------------------------------------------------------------

function isMetaKey(key) {
  return typeof key === "string" && key.startsWith(META_PREFIX);
}

// Objects stored before multi-user support have no owner tag and belong to the upload key.
function objectOwner(obj) {
  const owner = obj?.customMetadata?.owner;
  return typeof owner === "string" && owner ? owner : OWNER_ID;
}

// --- TTL settings -------------------------------------------------------------------------

// Accepts only the documented policy: 0 (never delete) or 60s..365d.
function normalizeTtlSeconds(value) {
  if (!Number.isInteger(value)) return null;
  if (value === 0) return 0;
  if (value >= MIN_TTL_SECONDS && value <= MAX_TTL_SECONDS) return value;
  return null;
}

// Reads the worker settings object; a missing, unreadable, or corrupt object means "defaults".
async function readSettings(env) {
  const fallback = { ttlSeconds: DEFAULT_TTL_SECONDS };
  let obj = null;
  try {
    obj = await env.CLIPS_BUCKET.get(SETTINGS_KEY);
  } catch {
    return fallback;
  }
  if (!obj) return fallback;
  try {
    const parsed = JSON.parse(await obj.text());
    const ttlSeconds = normalizeTtlSeconds(parsed?.ttlSeconds);
    if (ttlSeconds === null) return fallback;
    return { ...parsed, ttlSeconds };
  } catch {
    return fallback;
  }
}

// --- Friend invites -----------------------------------------------------------------------

// Reads the invite list; a missing, unreadable, or corrupt object means "no friends". Every entry
// is re-normalized because the object is plain JSON written by an earlier worker version.
async function readInvites(env) {
  const fallback = { friends: [] };
  let obj = null;
  try {
    obj = await env.CLIPS_BUCKET.get(INVITES_KEY);
  } catch {
    return fallback;
  }
  if (!obj) return fallback;
  try {
    const parsed = JSON.parse(await obj.text());
    const raw = Array.isArray(parsed?.friends) ? parsed.friends : [];
    const friends = [];
    for (const entry of raw) {
      if (!entry || typeof entry !== "object") continue;
      if (typeof entry.id !== "string" || !entry.id) continue;
      if (typeof entry.tokenHash !== "string" || !entry.tokenHash) continue;
      const ttlSeconds = normalizeTtlSeconds(entry.ttlSeconds);
      friends.push({
        id: entry.id,
        label: typeof entry.label === "string" && entry.label ? entry.label : DEFAULT_FRIEND_LABEL,
        tokenHash: entry.tokenHash,
        ttlSeconds: ttlSeconds === null ? DEFAULT_TTL_SECONDS : ttlSeconds,
        quotaBytes: normalizeQuotaBytes(entry.quotaBytes),
        createdAt: Number.isFinite(Number(entry.createdAt)) ? Number(entry.createdAt) : 0,
      });
    }
    return { friends };
  } catch {
    return fallback;
  }
}

async function writeInvites(env, invites) {
  await env.CLIPS_BUCKET.put(
    INVITES_KEY,
    JSON.stringify({ friends: invites.friends || [] }),
    { httpMetadata: { contentType: "application/json" } },
  );
}

// Effective TTL for an object's owner. A revoked friend id falls back to the owner policy so its
// clips keep a finite lifetime instead of being orphaned forever.
function ttlForOwner(ownerId, settings, invites) {
  const fallback = settings?.ttlSeconds ?? DEFAULT_TTL_SECONDS;
  if (!ownerId || ownerId === OWNER_ID) return fallback;
  const friend = invites?.friends?.find((entry) => entry.id === ownerId);
  return friend ? friend.ttlSeconds : fallback;
}

// Seconds left before the object expires. `Infinity` means "never". Retroactive by design: it
// compares the object's upload time against the *current* setting, so shortening the TTL expires
// existing clips immediately without rewriting them.
function remainingTtlSeconds(obj, ttlSeconds, nowMs = Date.now()) {
  if (!ttlSeconds || ttlSeconds <= 0) return Infinity;
  const uploadedMs =
    obj?.uploaded instanceof Date ? obj.uploaded.getTime() : Number(obj?.uploaded);
  if (!Number.isFinite(uploadedMs)) return ttlSeconds;
  return ttlSeconds - (nowMs - uploadedMs) / 1000;
}

function mediaCacheControl(ttlSeconds, remainingSeconds) {
  if (!ttlSeconds || ttlSeconds <= 0 || !Number.isFinite(remainingSeconds)) {
    return NEVER_MEDIA_CACHE;
  }
  return `public, max-age=${Math.min(MEDIA_CACHE_MAX_AGE, Math.max(1, Math.floor(remainingSeconds)))}`;
}

function pageCacheControl(ttlSeconds, remainingSeconds) {
  if (!ttlSeconds || ttlSeconds <= 0 || !Number.isFinite(remainingSeconds)) {
    return NEVER_PAGE_CACHE;
  }
  return `public, max-age=${Math.min(PAGE_CACHE_MAX_AGE, Math.max(1, Math.floor(remainingSeconds)))}`;
}

// --- Bucket listing -----------------------------------------------------------------------

async function listAllObjects(env) {
  const objects = [];
  let cursor;
  for (;;) {
    const page = await env.CLIPS_BUCKET.list({
      limit: LIST_LIMIT,
      cursor,
      // Ownership lives in customMetadata; without this the listing omits it and every object
      // looks like it belongs to the upload key.
      include: ["customMetadata"],
    });
    if (Array.isArray(page?.objects)) {
      objects.push(...page.objects);
    }
    if (!page?.truncated || !page.cursor) {
      return objects;
    }
    cursor = page.cursor;
  }
}

// Deletes keys in batches of 1,000 (the R2 per-call maximum for multi-delete).
async function deleteKeys(env, keys) {
  let deleted = 0;
  for (let i = 0; i < keys.length; i += LIST_LIMIT) {
    const batch = keys.slice(i, i + LIST_LIMIT);
    await env.CLIPS_BUCKET.delete(batch);
    deleted += batch.length;
  }
  return deleted;
}

// --- Admin --------------------------------------------------------------------------------

async function handleAdmin(request, env, ctx, url) {
  const { auth, response: authFailure } = await resolveAuth(request, env);
  if (authFailure) {
    return authFailure;
  }

  const path = url.pathname;
  if (path === "/admin/stats") {
    if (request.method !== "GET") {
      return methodNotAllowed("GET");
    }
    return adminStats(env, auth);
  }
  if (path === "/admin/settings") {
    if (request.method !== "PUT" && request.method !== "POST") {
      return methodNotAllowed("PUT, POST");
    }
    return adminUpdateSettings(request, env, auth);
  }
  if (path === "/admin/clips") {
    if (request.method !== "DELETE") {
      return methodNotAllowed("DELETE");
    }
    return adminDeleteClips(env, ctx, url, auth);
  }
  if (path === "/admin/invites") {
    if (request.method !== "POST") {
      return methodNotAllowed("POST");
    }
    return adminCreateInvite(request, env, auth);
  }
  if (path.startsWith("/admin/invites/")) {
    if (request.method !== "DELETE") {
      return methodNotAllowed("DELETE");
    }
    return adminRevokeInvite(env, ctx, url, auth, path.slice("/admin/invites/".length));
  }
  return json({ error: "Not found" }, 404);
}

// Every caller sees the bucket totals plus their own usage; only the owner gets the friend list,
// and it never carries token hashes.
async function adminStats(env, auth) {
  const [settings, invites, objects] = await Promise.all([
    readSettings(env),
    readInvites(env),
    listAllObjects(env),
  ]);
  const now = Date.now();
  const ownTtlSeconds =
    auth.role === "friend" ? auth.friend.ttlSeconds : settings.ttlSeconds;
  const perOwner = new Map();
  const bucket = { usedBytes: 0, clipCount: 0, objectCount: 0 };
  const own = { usedBytes: 0, clipCount: 0, objectCount: 0, expiredCount: 0 };

  for (const obj of objects) {
    if (isMetaKey(obj.key)) continue;
    const size = Number(obj.size) || 0;
    const isClip = obj.key.endsWith(".mp4");
    const ownerId = objectOwner(obj);

    bucket.usedBytes += size;
    bucket.objectCount += 1;
    if (isClip) bucket.clipCount += 1;

    let entry = perOwner.get(ownerId);
    if (!entry) {
      entry = { usedBytes: 0, clipCount: 0, objectCount: 0 };
      perOwner.set(ownerId, entry);
    }
    entry.usedBytes += size;
    entry.objectCount += 1;
    if (isClip) entry.clipCount += 1;

    if (ownerId === auth.ownerId) {
      own.usedBytes += size;
      own.objectCount += 1;
      if (isClip) own.clipCount += 1;
      if (remainingTtlSeconds(obj, ownTtlSeconds, now) <= 0) own.expiredCount += 1;
    }
  }

  const friends =
    auth.role === "owner"
      ? invites.friends.map((friend) => {
          const usage = perOwner.get(friend.id) || {
            usedBytes: 0,
            clipCount: 0,
            objectCount: 0,
          };
          return {
            id: friend.id,
            label: friend.label,
            ttlSeconds: friend.ttlSeconds,
            quotaBytes: friend.quotaBytes,
            createdAt: friend.createdAt,
            usedBytes: usage.usedBytes,
            clipCount: usage.clipCount,
            objectCount: usage.objectCount,
          };
        })
      : [];

  return json({
    ok: true,
    role: auth.role,
    label: auth.role === "friend" ? auth.friend.label : null,
    usedBytes: own.usedBytes,
    clipCount: own.clipCount,
    objectCount: own.objectCount,
    expiredCount: own.expiredCount,
    bucketUsedBytes: bucket.usedBytes,
    bucketClipCount: bucket.clipCount,
    bucketObjectCount: bucket.objectCount,
    freeTierBytes: FREE_TIER_BYTES,
    quotaBytes: auth.role === "friend" ? auth.friend.quotaBytes : 0,
    ttlSeconds: ownTtlSeconds,
    friends,
  });
}

// The owner's TTL is a bucket-wide policy; a friend can only shorten (or reset) their own copy.
async function adminUpdateSettings(request, env, auth) {
  let body;
  try {
    body = await request.json();
  } catch {
    return json({ error: "Body must be JSON" }, 400);
  }

  const ttlSeconds = normalizeTtlSeconds(body?.ttlSeconds);
  if (ttlSeconds === null) {
    return json(
      {
        error: `ttlSeconds must be 0 (never) or an integer between ${MIN_TTL_SECONDS} and ${MAX_TTL_SECONDS}`,
      },
      400,
    );
  }

  if (auth.role === "friend") {
    const invites = await readInvites(env);
    const friend = invites.friends.find((entry) => entry.id === auth.ownerId);
    if (!friend) {
      return json({ error: "Unauthorized" }, 401);
    }
    friend.ttlSeconds = ttlSeconds;
    await writeInvites(env, invites);
    return json({ ok: true, ttlSeconds });
  }

  await env.CLIPS_BUCKET.put(
    SETTINGS_KEY,
    JSON.stringify({ ttlSeconds, updatedAt: Date.now() }),
    { httpMetadata: { contentType: "application/json" } },
  );
  return json({ ok: true, ttlSeconds });
}

// `scope=mine` (default) touches only the caller's objects; `scope=all` is owner-only.
async function adminDeleteClips(env, ctx, url, auth) {
  const scope = url.searchParams.get("scope") || "mine";
  if (scope !== "mine" && scope !== "all") {
    return json({ error: 'scope must be "mine" or "all"' }, 400);
  }
  if (scope === "all" && auth.role !== "owner") {
    return json({ error: "Only the owner can delete everyone's clips" }, 403);
  }

  const keys = (await listAllObjects(env))
    .filter((obj) => !isMetaKey(obj.key))
    .filter((obj) => scope === "all" || objectOwner(obj) === auth.ownerId)
    .map((obj) => obj.key);

  const deletedClips = keys.filter((key) => key.endsWith(".mp4")).length;
  const deletedObjects = await deleteKeys(env, keys);
  await purgeEdgeCache(ctx, url, keys);

  return json({ ok: true, scope, deletedObjects, deletedClips });
}

async function adminCreateInvite(request, env, auth) {
  const forbidden = ownerOnly(auth);
  if (forbidden) return forbidden;

  let body;
  try {
    body = await request.json();
  } catch {
    return json({ error: "Body must be JSON" }, 400);
  }

  let label = DEFAULT_FRIEND_LABEL;
  if (body?.label !== undefined && body?.label !== null) {
    if (typeof body.label !== "string") {
      return json({ error: "label must be a string" }, 400);
    }
    label = body.label.trim() || DEFAULT_FRIEND_LABEL;
    if (label.length > FRIEND_LABEL_MAX) {
      return json(
        { error: `label must be at most ${FRIEND_LABEL_MAX} characters` },
        400,
      );
    }
  }

  const settings = await readSettings(env);
  let ttlSeconds = settings.ttlSeconds;
  if (body?.ttlSeconds !== undefined) {
    ttlSeconds = normalizeTtlSeconds(body.ttlSeconds);
    if (ttlSeconds === null) {
      return json(
        {
          error: `ttlSeconds must be 0 (never) or an integer between ${MIN_TTL_SECONDS} and ${MAX_TTL_SECONDS}`,
        },
        400,
      );
    }
  }

  let quotaBytes = 0;
  if (body?.quotaBytes !== undefined && body?.quotaBytes !== null) {
    quotaBytes = parseQuotaBytes(body.quotaBytes);
    if (quotaBytes === null) {
      return json({ error: "quotaBytes must be an integer >= 0" }, 400);
    }
  }

  const invites = await readInvites(env);
  const token = base64UrlEncode(crypto.getRandomValues(new Uint8Array(TOKEN_BYTES)));
  const entry = {
    id: `f_${randomHex(4)}`,
    label,
    tokenHash: await sha256Hex(token),
    ttlSeconds,
    quotaBytes,
    createdAt: Date.now(),
  };
  invites.friends.push(entry);
  await writeInvites(env, invites);

  // The plaintext token is returned exactly once; only its hash is persisted.
  return json({
    ok: true,
    id: entry.id,
    label: entry.label,
    token,
    ttlSeconds: entry.ttlSeconds,
    quotaBytes: entry.quotaBytes,
    createdAt: entry.createdAt,
  });
}

async function adminRevokeInvite(env, ctx, url, auth, id) {
  const forbidden = ownerOnly(auth);
  if (forbidden) return forbidden;

  const invites = await readInvites(env);
  const index = invites.friends.findIndex((friend) => friend.id === id);
  if (index === -1) {
    return json({ error: "Invite not found" }, 404);
  }
  invites.friends.splice(index, 1);
  await writeInvites(env, invites);

  let deletedObjects = 0;
  let deletedClips = 0;
  if (url.searchParams.get("deleteClips") === "1") {
    const keys = (await listAllObjects(env))
      .filter((obj) => !isMetaKey(obj.key) && objectOwner(obj) === id)
      .map((obj) => obj.key);
    deletedClips = keys.filter((key) => key.endsWith(".mp4")).length;
    deletedObjects = await deleteKeys(env, keys);
    await purgeEdgeCache(ctx, url, keys);
  }

  return json({ ok: true, id, deletedObjects, deletedClips });
}

function ownerOnly(auth) {
  if (auth.role === "owner") return null;
  return json({ error: "Only the owner can manage invites" }, 403);
}

function parseQuotaBytes(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function normalizeQuotaBytes(value) {
  const parsed = parseQuotaBytes(Number(value));
  return parsed === null ? 0 : parsed;
}

function randomHex(byteLength) {
  let hex = "";
  for (const byte of crypto.getRandomValues(new Uint8Array(byteLength))) {
    hex += byte.toString(16).padStart(2, "0");
  }
  return hex;
}

function base64UrlEncode(bytes) {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

// Best-effort edge purge: a stale cache entry would otherwise outlive the deleted objects.
async function purgeEdgeCache(ctx, url, keys) {
  if (!edgeCache || keys.length === 0) return;
  const slugs = new Set(
    keys.map((key) => (key.endsWith(".mp4") || key.endsWith(".png") ? key.slice(0, -4) : key)),
  );
  const purges = [];
  for (const slug of slugs) {
    for (const path of [`/${slug}`, `/${slug}.mp4`, `/${slug}.png`]) {
      purges.push(edgeCache.delete(`${url.origin}${path}`).catch(() => false));
    }
  }
  const all = Promise.all(purges).catch(() => {});
  if (ctx?.waitUntil) {
    ctx.waitUntil(all);
  } else {
    await all;
  }
}

// --- Cron ---------------------------------------------------------------------------------

async function deleteExpiredObjects(env) {
  const [settings, invites] = await Promise.all([readSettings(env), readInvites(env)]);
  // Only skip the hourly scan when no owner can ever expire: nobody is on a finite TTL.
  if (
    settings.ttlSeconds <= 0 &&
    invites.friends.every((friend) => friend.ttlSeconds <= 0)
  ) {
    return;
  }

  const now = Date.now();
  const expired = [];
  for (const obj of await listAllObjects(env)) {
    if (isMetaKey(obj.key)) continue;
    const ttlSeconds = ttlForOwner(objectOwner(obj), settings, invites);
    if (remainingTtlSeconds(obj, ttlSeconds, now) <= 0) {
      expired.push(obj.key);
    }
  }

  await deleteKeys(env, expired);
}

function methodNotAllowed(allow) {
  return json({ error: "Method not allowed" }, 405, { Allow: allow });
}

function parseRange(header, size) {
  if (!size || !header.startsWith("bytes=")) return null;
  const spec = header.slice(6).split(",")[0].trim();
  const dash = spec.indexOf("-");
  if (dash === -1) return null;

  const startStr = spec.slice(0, dash).trim();
  const endStr = spec.slice(dash + 1).trim();

  if (startStr === "") {
    if (!/^\d+$/.test(endStr)) return null;
    const suffix = Number(endStr);
    if (!Number.isSafeInteger(suffix) || suffix <= 0) return null;
    const start = Math.max(0, size - suffix);
    return { start, end: size - 1 };
  }

  if (!/^\d+$/.test(startStr)) return null;
  const start = Number(startStr);
  if (!Number.isSafeInteger(start) || start >= size) return null;

  if (endStr === "") {
    return { start, end: size - 1 };
  }

  if (!/^\d+$/.test(endStr)) return null;
  const end = Math.min(Number(endStr), size - 1);
  if (!Number.isSafeInteger(end) || end < start) return null;
  return { start, end };
}

function parseContentLength(val) {
  if (val === null) return null;
  const trimmed = val.trim();
  if (!/^\d{1,15}$/.test(trimmed)) return null;
  const n = Number(trimmed);
  return Number.isSafeInteger(n) && n >= 1 ? n : null;
}

function parsePositiveInt(val) {
  if (!val || !/^\d+$/.test(val.trim())) return null;
  const n = Number(val.trim());
  return Number.isSafeInteger(n) && n > 0 ? n : null;
}

function parseDimension(val) {
  const n = parsePositiveInt(val);
  return n && n <= 16384 ? n : null;
}

// Strips ASCII control characters and Unicode BiDi controls so titles cannot spoof or break
// headers, embeds, or the page.
function sanitizeTitle(val) {
  if (!val) return "";
  let out = "";
  for (const ch of String(val)) {
    if (ch === "\t" || ch === "\r" || ch === "\n") {
      out += " ";
      continue;
    }
    const cp = ch.codePointAt(0);
    if (cp <= 0x1f || cp === 0x7f) {
      continue;
    }
    if (BIDI_CONTROL_RE.test(ch)) {
      continue;
    }
    out += ch;
  }
  return out.replace(/\s+/g, " ").trim().slice(0, 120);
}

function tryDecode(val) {
  if (!val) return "";
  try {
    return decodeURIComponent(val);
  } catch {
    return val;
  }
}

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function escapeDiscordMarkdown(s) {
  return String(s).replace(/([\\*_~`<>|#[\]()])/g, "\\$1");
}

function serializeComponentEmbed(title, pageUrl, videoUrl) {
  const payload = {
    component: {
      type: 17,
      components: [
        {
          type: 10,
          content: `### [${escapeDiscordMarkdown(title)}](${pageUrl})`,
        },
        {
          type: 12,
          items: [{ media: { url: videoUrl } }],
        },
      ],
    },
  };
  // Escape HTML-significant chars so titles cannot break out of the inline <script>.
  return JSON.stringify(payload)
    .replace(/</g, "\\u003c")
    .replace(/>/g, "\\u003e")
    .replace(/&/g, "\\u0026")
    .replace(/\u2028/g, "\\u2028")
    .replace(/\u2029/g, "\\u2029");
}

function buildComponentEmbedTag(title, pageUrl, videoUrl) {
  const safePageUrl = String(pageUrl);
  const safeVideoUrl = String(videoUrl);
  let jsonText = serializeComponentEmbed(title, safePageUrl, safeVideoUrl);

  if (TEXT_ENCODER.encode(jsonText).byteLength > COMPONENT_EMBED_MAX_BYTES) {
    let fitted = null;
    for (let len = Math.min(title.length, 120); len >= 1; len -= 10) {
      const candidate = serializeComponentEmbed(`${title.slice(0, len)}...`, safePageUrl, safeVideoUrl);
      if (TEXT_ENCODER.encode(candidate).byteLength <= COMPONENT_EMBED_MAX_BYTES) {
        fitted = candidate;
        break;
      }
    }
    if (!fitted) {
      return "";
    }
    jsonText = fitted;
  }

  return `  <script id="discord:component-embed" type="application/json">${jsonText}</script>\n`;
}

function withSecurityHeaders(headers) {
  for (const [name, value] of Object.entries(SECURITY_HEADERS)) {
    headers.set(name, value);
  }
  return headers;
}

function notFound() {
  return new Response("Not found", {
    status: 404,
    headers: withSecurityHeaders(new Headers({ "Content-Type": "text/plain; charset=utf-8" })),
  });
}

function json(data, status = 200, extraHeaders = {}) {
  return new Response(JSON.stringify(data), {
    status,
    headers: withSecurityHeaders(
      new Headers({
        "Content-Type": "application/json; charset=utf-8",
        ...extraHeaders,
      }),
    ),
  });
}
