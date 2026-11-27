# clip-share worker

Cloudflare Worker + R2 backend for sharing clips at `https://clips.yourdomain.com/<clipname>` with Discord OpenGraph video embeds and HTTP `Range` streaming.

## Setup

1. Create the R2 bucket (once):
   ```bash
   npx wrangler r2 bucket create clip-shares
   ```
2. Auto-delete rule (3 days):
   ```bash
   npx wrangler r2 bucket lifecycle add clip-shares expire-3d --expire-days 3
   ```
3. Set your upload secret (enter the same key in `clip` Settings -> Upload key):
   ```bash
   npx wrangler secret put UPLOAD_KEY
   ```
4. Uncomment `routes` in `wrangler.toml` with your `clips.<yourdomain>` hostname, then deploy:
   ```bash
   npx wrangler deploy
   ```
