-- JSON: {"dmarc","spf","dkim","spam_score"} from Cloudflare's MX, or NULL when unavailable (e.g. sent mail).
ALTER TABLE messages ADD COLUMN auth TEXT;
