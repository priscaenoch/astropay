# ASTROpay Rust Backend

This service is the beginning of the backend migration out of Next.js route handlers.

What it currently owns:

- merchant registration, login, logout, and cookie-backed sessions
- invoice creation, listing, detail lookup, and status lookup
- Horizon-backed reconciliation for pending invoices
- webhook-driven payment marking (`/api/webhooks/stellar`)
- a Rust migration runner that reuses the existing SQL migrations

What is intentionally not faked yet:

- buyer XDR generation/submission for checkout
- merchant settlement cron

Those routes return `501 Not Implemented` in the Rust service until the Stellar transaction logic is ported properly.

## HTTP 401 (authentication) JSON contract

All **401 Unauthorized** responses use a single structured shape so clients can branch on `error.code` instead of parsing free-form strings:

```json
{
  "error": {
    "code": "AUTH_INVALID_CREDENTIALS",
    "message": "Invalid credentials"
  }
}
```

Stable `code` values (do not depend on `message` for logic):

| `code` | When |
| --- | --- |
| `AUTH_INVALID_CREDENTIALS` | Login rejected (unknown email or wrong password; same response for both). |
| `AUTH_SESSION_REQUIRED` | Cookie session missing, invalid, expired, or not accepted for the route (e.g. `/api/auth/me`, invoice routes). |
| `AUTH_CRON_SECRET_MISMATCH` | `Authorization: Bearer …` does not match `CRON_SECRET` for cron/webhook routes. |

Other HTTP errors (400, 404, 409, 501, 500) still use the legacy form `{ "error": "<string>" }` until migrated.

**Quick check:** with the server running, `curl -s -o /dev/stderr -w "%{http_code}" http://127.0.0.1:8080/api/auth/me` should print `401` and a JSON body whose `error.code` is `AUTH_SESSION_REQUIRED`.

## Run locally

```bash
cd rust-backend
cargo run --bin migrate
cargo run
```

The service reads env vars from:

- `rust-backend/.env.local`
- `rust-backend/.env`
- `../usdc-payment-link-tool/.env.local`
- `../usdc-payment-link-tool/.env`
