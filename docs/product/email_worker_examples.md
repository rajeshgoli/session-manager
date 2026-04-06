## Email Workers

Session Manager supports two inbound email routing shapes:

- `reply@sm.rajeshgo.li`: shared mailbox routing that extracts the `SM:` footer from the quoted thread
- `session-id@sm.rajeshgo.li`: explicit id-addressed routing that trusts `x-email-session-id` only when the worker secret header is valid

### Id-Addressed Worker

Tracked example:

- `examples/cloudflare/email_worker_id_routing.js`

This worker is meant for a Cloudflare Email Routing catch-all on `*@sm.rajeshgo.li`.

Behavior:

- normalizes `message.from`
- rejects senders not present in `ALLOWED_SENDERS`
- rejects recipients whose local-part does not match the expected Session Manager id shape
- forwards `raw_email`
- sets `x-email-worker-secret`
- sets `x-email-session-id` so Session Manager can skip footer parsing and route directly

Required worker variables:

- `ALLOWED_SENDERS=rajeshgoli@gmail.com`
- `SM_WEBHOOK_URL=https://sm.rajeshgo.li/api/email-inbound`
- `EMAIL_WORKER_SECRET=<same value as email_bridge.worker_secret>`

Required Session Manager config:

```yaml
email_bridge:
  authorized_senders:
    - "rajeshgoli@gmail.com"
  worker_secret: "<same value as EMAIL_WORKER_SECRET>"
  worker_secret_header: "x-email-worker-secret"
  session_id_header: "x-email-session-id"
  webhook_path: "/api/email-inbound"
```

Use this mode only when Cloudflare Email Routing can deliver a catch-all or equivalent address pattern to the worker. If catch-all is unavailable, use the shared `reply@sm.rajeshgo.li` footer-routing mode instead.
