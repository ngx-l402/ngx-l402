# Production Deployment (TLS)

The install examples run ngx-l402 as plain HTTP on `:8000` and assume something in
front terminates TLS. But the official image is built on the nginx image, which
ships `http_ssl_module`, so **nginx can terminate TLS itself** — no separate
reverse proxy needed. This page shows the production pattern: serve `:443` with a
Let's Encrypt certificate that renews automatically.

---

## TLS server block

Point nginx at the certificate and enable your paywalled location on `:443`:

```nginx
server {
    listen 443 ssl;
    server_name blob.example.com;

    ssl_certificate     /etc/letsencrypt/live/blob.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/blob.example.com/privkey.pem;
    ssl_protocols       TLSv1.2 TLSv1.3;

    location /protected {
        l402 on;
        l402_amount_msat_default 1000;
        proxy_pass http://your-upstream;
    }
}
```

---

## Issuing and renewing the certificate

Run **certbot** alongside nginx to obtain the cert (standalone, on port 80) and
renew it on a schedule. Share the `/etc/letsencrypt` volume between the two:
certbot writes the cert, nginx reads it.

```yaml
services:
  certbot:
    image: certbot/certbot
    ports: ["80:80"]
    volumes: ["./certbot/conf:/etc/letsencrypt"]
    # obtain once, then renew twice a day
    entrypoint: >
      sh -c "certbot certonly --standalone -n --agree-tos -m you@example.com
             -d blob.example.com || true;
             while :; do certbot renew; sleep 12h; done"

  nginx-l402:
    image: ghcr.io/ngx-l402/ngx-l402:latest
    ports: ["443:443"]
    volumes: ["./certbot/conf:/etc/letsencrypt:ro"]   # nginx reads the cert
    # ... LN_CLIENT_TYPE, ROOT_KEY, etc.
```

---

## Picking up renewed certificates

nginx loads the certificate into memory at startup and **won't see a renewed one
until it reloads**. The simplest robust approach is a periodic reload — wrap nginx
so it reloads every few hours, then runs in the foreground:

```sh
{ while :; do sleep 6h & wait ${!}; nginx -s reload; done & nginx -g 'daemon off;'; }
```

**This does not cause downtime**, for two reasons:

- Let's Encrypt renews **~30 days before expiry** (certbot's default). So when the
  new cert appears, nginx is still holding one with ~30 days left — the ≤6h reload
  delay is nowhere near expiry, so no request ever meets an expired certificate.
- `nginx -s reload` is **graceful**: it starts new workers with the new cert and
  drains the old ones, so the reload itself drops no connections.

For zero staleness you can instead reload the instant a cert renews, via a certbot
`--deploy-hook` — but that has to signal nginx across container boundaries, so the
periodic reload is the simpler choice and, given the 30-day margin, just as safe.

---

## A complete working reference

The [`paywalled-blossom`](https://github.com/ngx-l402/ngx-l402/tree/main/examples/paywalled-blossom)
example wires all of this up end to end — TLS on `:443`, a certbot sidecar with
auto-renewal, and the reload loop — as a `docker compose up -d` deployment. Start
from it rather than assembling by hand.
