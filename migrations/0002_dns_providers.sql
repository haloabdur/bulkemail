-- How a domain's DNS records get published: 'manual' or 'cloudflare'.
ALTER TABLE domains ADD COLUMN dns_provider TEXT NOT NULL DEFAULT 'manual';
-- Cloudflare API token, encrypted with APP_SECRET (NULL = use CLOUDFLARE_API_TOKEN).
ALTER TABLE domains ADD COLUMN cf_api_token TEXT;
ALTER TABLE domains ADD COLUMN cf_zone_id TEXT;
ALTER TABLE domains ADD COLUMN cf_zone_name TEXT;
ALTER TABLE domains ADD COLUMN cf_synced_at INTEGER;
-- JSON array of the changes made in the last Cloudflare sync.
ALTER TABLE domains ADD COLUMN cf_last_changes TEXT NOT NULL DEFAULT '[]';
