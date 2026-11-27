# clip-share worker

Cloudflare Worker + R2 backend for sharing clips at `https://clips.yourdomain.com/<clipname>` with Discord OpenGraph video embeds and HTTP `Range` streaming.

## Setup

1. Create the R2 bucket (once):
   ```bash
   npx wrangler r2 bucket create clip-shares
   ```
2. Set your upload secret (enter the same key in the `clip` app's Cloudflare tab -> Upload key):
   ```bash
   npx wrangler secret put UPLOAD_KEY
   ```
3. Uncomment `routes` in `wrangler.toml` with your `clips.<yourdomain>` hostname, then deploy:
   ```bash
   npx wrangler deploy
   ```

`wrangler.toml` declares the hourly Cron trigger (`0 * * * *`) that deletes expired clips. Deploying registers it.

## Auto-delete TTL

Auto-delete is configured **in the `clip` app**, in the Cloudflare tab. The app writes the TTL to the worker (`PUT /admin/settings`) and the worker enforces it on every read, so a shortened TTL applies retroactively to clips that are already stored:

- `0` = never delete.
- Otherwise 60 seconds to 365 days, default 3 days when no setting has been saved.
- Every stored object is tagged with its uploader; the TTL applied is that uploader's current one (the owner's policy, or a friend's own override). Revoking a friend falls back to the owner policy.
- Expired clips return `404` immediately (no waiting for cleanup), and `Cache-Control` is capped to the remaining lifetime so the edge cache cannot serve them past the TTL.
- The hourly Cron trigger deletes expired objects from R2 in batches of 1,000. It only skips the listing when nobody has a finite TTL.

If you previously added an R2 lifecycle rule for auto-delete, remove it — the worker's Cron replaces it and a lifecycle rule would ignore the in-app setting:

```bash
npx wrangler r2 bucket lifecycle remove clip-shares --name expire-3d
```

## Account endpoints

All require the upload key (`Authorization: Bearer <key>` or `X-Upload-Key`), or a friend invite token.

- `GET /admin/stats` -> `{ ok, role, label, usedBytes, clipCount, objectCount, expiredCount, bucketUsedBytes, bucketClipCount, bucketObjectCount, freeTierBytes, quotaBytes, ttlSeconds, friends }` — `usedBytes`/`clipCount`/`objectCount`/`expiredCount` cover the caller only; `friends` is populated for the owner and never contains token hashes.
- `PUT|POST /admin/settings` with `{ "ttlSeconds": 259200 }` — the owner sets the bucket policy; a friend changes only their own TTL.
- `DELETE /admin/clips?scope=mine|all` — deletes the caller's clips (default `mine`); `all` is owner-only and leaves the settings and invite objects untouched.
- `POST /admin/invites` with `{ "label": "Ada", "ttlSeconds": 86400, "quotaBytes": 0 }` — owner only; `ttlSeconds` defaults to the owner policy and `quotaBytes: 0` means unlimited. Returns the invite plus its one-time `token` (only its hash is stored).
- `DELETE /admin/invites/<id>?deleteClips=1` — owner only; revokes the invite immediately and, with `deleteClips=1`, deletes that friend's clips.

`_meta/*` objects hold the worker's settings and invites and are never publicly routable.

## Tests

```bash
npm test
```
