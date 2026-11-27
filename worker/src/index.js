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
};

// Serves a clip response through the edge cache. Only plain GETs without a Range header are
// cacheable; all misses and hits are (re)stored in the background.
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
  if (response.status === 200) {
    ctx?.waitUntil(edgeCache.put(request, response.clone()));
  }
  return response;
}

async function handleUpload(request, env, url, rawName) {
  const expectedKey = typeof env.UPLOAD_KEY === "string" ? env.UPLOAD_KEY : "";
  if (!expectedKey) {
    return json({ error: "Worker UPLOAD_KEY is not configured" }, 503);
  }
  const providedKey = extractAuthKey(request);
  if (!providedKey || !(await timingSafeEqual(providedKey, expectedKey))) {
    return json({ error: "Unauthorized" }, 401);
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
  if (!request.body) {
    return json({ error: "Empty request body" }, 400);
  }

  const contentType = isThumb ? "image/png" : "video/mp4";
  const putOptions = {
    httpMetadata: {
      contentType,
      cacheControl: "public, max-age=31536000, immutable",
    },
    // Never silently replace an existing object.
    onlyIf: { etagDoesNotMatch: "*" },
  };
  if (isVideo) {
    const customMetadata = {};
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
    putOptions.customMetadata = customMetadata;
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
    const head = await env.CLIPS_BUCKET.head(key);
    if (!head) {
      return notFound();
    }
    const notModified = notModifiedResponse(request, head.httpEtag);
    if (notModified) {
      return notModified;
    }
    const headers = videoHeaders(head, key);
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

  // Single R2 roundtrip: let the bucket resolve the range instead of HEADing first.
  let obj = null;
  try {
    obj = await env.CLIPS_BUCKET.get(key, rangeHeader ? { range: request.headers } : undefined);
  } catch {
    obj = null;
  }
  if (!obj) {
    if (!rangeHeader) {
      return notFound();
    }
    const head = await env.CLIPS_BUCKET.head(key);
    if (!head) {
      return notFound();
    }
    const headers = videoHeaders(head, key);
    headers.set("Content-Range", `bytes */${head.size}`);
    return new Response(null, { status: 416, headers });
  }

  const notModified = notModifiedResponse(request, obj.httpEtag);
  if (notModified) {
    return notModified;
  }

  if (rangeHeader) {
    const parsed = parseRange(rangeHeader, obj.size);
    if (!parsed) {
      const headers = videoHeaders(obj, key);
      headers.set("Content-Range", `bytes */${obj.size}`);
      return new Response(null, { status: 416, headers });
    }
    if (obj.range && typeof obj.range.offset === "number") {
      const offset = obj.range.offset;
      const length =
        typeof obj.range.length === "number" ? obj.range.length : obj.size - offset;
      const headers = videoHeaders(obj, key);
      headers.set("Content-Range", `bytes ${offset}-${offset + length - 1}/${obj.size}`);
      headers.set("Content-Length", String(length));
      return new Response(obj.body, { status: 206, headers });
    }
    // The bucket ignored the range and handed back the whole object.
  }

  const headers = videoHeaders(obj, key);
  headers.set("Content-Length", String(obj.size));
  return new Response(obj.body, { status: 200, headers });
}

async function handleThumb(request, env, key) {
  if (request.method === "HEAD") {
    const head = await env.CLIPS_BUCKET.head(key);
    if (!head) {
      return notFound();
    }
    const notModified = notModifiedResponse(request, head.httpEtag);
    if (notModified) {
      return notModified;
    }
    const headers = thumbHeaders(head);
    headers.set("Content-Length", String(head.size));
    return new Response(null, { status: 200, headers });
  }

  const obj = await env.CLIPS_BUCKET.get(key);
  if (!obj) {
    return notFound();
  }
  const notModified = notModifiedResponse(request, obj.httpEtag);
  if (notModified) {
    return notModified;
  }
  const headers = thumbHeaders(obj);
  headers.set("Content-Length", String(obj.size));
  return new Response(obj.body, { status: 200, headers });
}

function videoHeaders(obj, key) {
  const headers = new Headers();
  obj.writeHttpMetadata(headers);
  headers.set("Content-Type", "video/mp4");
  headers.set("Accept-Ranges", "bytes");
  headers.set("ETag", obj.httpEtag);
  headers.set("Cache-Control", "public, max-age=31536000, immutable");
  headers.set("Content-Disposition", `inline; filename="${key}"`);
  return withSecurityHeaders(headers);
}

function thumbHeaders(obj) {
  const headers = new Headers();
  obj.writeHttpMetadata(headers);
  headers.set("Content-Type", "image/png");
  headers.set("ETag", obj.httpEtag);
  headers.set("Cache-Control", "public, max-age=31536000, immutable");
  return withSecurityHeaders(headers);
}

function notModifiedResponse(request, etag) {
  if (!etagMatches(request.headers.get("If-None-Match"), etag)) {
    return null;
  }
  const headers = new Headers();
  headers.set("ETag", etag);
  headers.set("Cache-Control", "public, max-age=31536000, immutable");
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
  const head = await env.CLIPS_BUCKET.head(`${slug}.mp4`);
  if (!head) {
    return new Response("Clip not found", {
      status: 404,
      headers: withSecurityHeaders(
        new Headers({ "Content-Type": "text/plain; charset=utf-8" }),
      ),
    });
  }

  const headers = new Headers({
    "Content-Type": "text/html; charset=utf-8",
    "Cache-Control": "public, max-age=300",
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
