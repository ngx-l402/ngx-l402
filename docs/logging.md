# Logging

## View Logs

### systemd / Manual Install

```bash
# Module initialization and system logs
sudo journalctl -u nginx

# Nginx error logs (real-time)
sudo tail -f /var/log/nginx/error.log

# Cashu redemption logs
sudo tail -f /var/log/nginx/cashu_redemption.log
```

### Docker

```bash
docker logs l402-nginx -f
```

---

## Log Levels

Control verbosity via the `RUST_LOG` environment variable:

```bash
# Standard info logs (recommended for production)
Environment=RUST_LOG=info

# Detailed debug logs for all modules
Environment=RUST_LOG=debug

# Module-specific debug logs only (reduces noise)
Environment=RUST_LOG=ngx_l402_lib=debug,info
```

---

## Structured JSON Logs

Set `l402_log_format json;` on a protected location to emit one JSON line per L402 access event, alongside the usual text logs. Field names match the dry-run line, so the same `jq` / log-aggregator queries work for both:

```json
{"event":"l402_verify","route":"/api/data","backend":"lnd","client_ip":"203.0.113.7","auth_state":"valid","method":"lightning","latency_ms":42}
```

Events: `l402_verify` (`auth_state` valid or invalid, `method` when known), `l402_challenge` (a 402 was issued, with `price_msat` and `price_source`), `l402_challenge_error` (`error`: `backend`, `timeout`, or `panic`), and `l402_rate_limited` (a 429, with `window_secs`).

Off by default (`text`), so existing deployments see no change. The directive is per-location: an inner `l402_log_format text;` overrides an outer `json`.
