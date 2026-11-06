# batch_downloader

## Operations

`/metrics` accepts `Authorization: Bearer <API_KEY>` or the legacy
`Authorization: <API_KEY>` header, like the rest of
the authenticated API — it is no longer reachable anonymously. Point your
Prometheus scrape config's `authorization` (or equivalent bearer/header
config) at the same `API_KEY` used for `/api/*`.

### Task/archive persistence and expiry

Tasks (and the `/tmp` archive files backing them) live only in an in-memory
cache — they are **not persisted across restarts**. On a restart/deploy, any
task that existed before is gone, and its archive file (if any) is cleaned
up on startup. This means `check_archive`/`download` for a task created
before a restart will return `404 Not Found` even if the task previously
existed or completed. `book_bot` (or any client) should treat a `404` from
`check_archive` as "task unknown/expired — please retry the request" rather
than as a hard error, since the batch_downloader offers at-most-once
semantics for a given task lifetime, not durable storage.

Independent of restarts, a completed task's download link also has a hard
expiry: **24 hours** from creation, after which the task and its archive
are evicted and the link stops working (see `TASK_RESULT_TTL` in
`src/views.rs`), regardless of how recently it was polled.

### Configuration

| Variable | Default | Meaning |
|---|---|---|
| `SENTRY_DSN` | unset | Optional; Sentry disabled when empty |
| `CACHE_REQUEST_TIMEOUT_SECS` | 900 | Positive per-request timeout (headers and body), including TFCS cold fills |
| `MAX_CONCURRENT_TASKS` | 4 | Archive tasks running in parallel |
| `MAX_QUEUED_TASKS` | 32 | Tasks waiting for a slot; beyond that `POST /api/` returns 503 |
| `DATA_DIR` | `/tmp/batch_downloader` | Directory for archives; exclusively locked before startup cleanup (lock file preserved) |

The task idle watchdog is derived from `CACHE_REQUEST_TIMEOUT_SECS + 60`
seconds, so a legitimate cold fill does not exhaust the idle window.
