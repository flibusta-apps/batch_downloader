# batch_downloader

## Operations

`/metrics` requires the same `Authorization: <API_KEY>` header as the rest of
the authenticated API — it is no longer reachable anonymously. Point your
Prometheus scrape config's `authorization` (or equivalent bearer/header
config) at the same `API_KEY` used for `/api/*`.
