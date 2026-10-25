# Nano

The module can accept a [Nano](https://docs.nano.org) payment as an additional
payment method, next to Lightning and Cashu. It is off unless you configure it,
and with it off nothing about the 402 response changes.

The module never holds Nano keys and never sends funds. It only checks, through
a Nano node's RPC, that a payment the client already made is a confirmed send to
your address for exactly the amount its 402 challenge asked for.

---

## Configuration

| Variable | Required | Description |
|---|---|---|
| `NANO_ADDRESS` | ✅ | Your receiving address (`nano_…` or `xrb_…`). Checked against its checksum at startup; an invalid one disables Nano and logs why |
| `NANO_RAW_PER_SAT` | ✅ | Fixed conversion, in raw per sat (1 XNO = 10^30 raw). The route price in msat is converted with it and rounded up. Without it there is no Nano offer |
| `NANO_RPC_URL` | ✅ | `http(s)` URL of a node RPC that answers `block_info` (your own node, or a provider you trust) |
| `NANO_RPC_TIMEOUT_MS` | — | Bound on one RPC call, 1–60000, default 5000 |
| `NANO_CHALLENGE_TTL_SECS` | — | How long a challenge can be paid and redeemed, 30–86400, default 600 |

All of them are read once at startup, like the other variables. Add `env NANO_ADDRESS;`
etc. to `nginx.conf` if you start nginx without systemd. Challenges are signed
with a key derived from `ROOT_KEY`; without `ROOT_KEY` there is no Nano offer.

---

## Flow

1. An unpaid request gets the usual 402, plus

   ```
   X-Nano: nano:<NANO_ADDRESS>?amount=<raw>
   X-Nano-Challenge: <challenge id>
   ```

   `<raw>` is the route price plus a per-challenge tag of at most 2^40 raw
   (about 10^-18 XNO), so every 402 asks for its own amount.

2. The client sends exactly `<raw>` to that address and waits for confirmation.
3. It retries the same request with the challenge id and the hash of its send
   block:

   ```bash
   curl -i https://your-gateway/protected \
     -H "Authorization: Nano <challenge id>:991CF190094C00F0B68E2E5F75F6BEE95A2E0BD93CEAA4A6734DB9F19B728948"
   ```

4. The module checks the challenge id first: it must have been issued by this
   gateway for the same request binding (path and method, or realm and method,
   as for macaroons) and the same price, and must not be expired. Then it POSTs
   `{"action":"block_info","json_block":"true","hash":…}` to `NANO_RPC_URL` and
   admits the request only if the reply says `confirmed` is `"true"`, `subtype`
   is `"send"`, `contents.link_as_account` is your address and `amount` is
   exactly the challenge amount.

One send block pays one challenge, and one challenge is paid by one block. Both
are recorded as used in one atomic step (`l402:nano:<HASH>` with no expiry and
`l402:nano_challenge:<nonce>` until the challenge expires, in Redis) and are
refused if presented again.

### Why the challenge

A send block is public as soon as it is published, so its hash is not a secret.
Without the binding, anyone watching your address could present a payer's block
hash before the payer did. With it, a block only redeems the challenge whose tag
its amount carries, and only together with that challenge's id, which only the
client that received the 402 has seen.

The challenge id is signed rather than stored, so any worker can check it and
no state is written until a payment is accepted. A change of price, or of the
request, between the 402 and the retry invalidates it.

---

## Failure handling

| Case | Status |
|---|---|
| Malformed credential, challenge not for this request or price, expired challenge, unknown block, unconfirmed, not a send, wrong destination, amount other than the challenge amount, block or challenge already used | 401 |
| RPC unreachable, non-2xx, timed out or oversized reply; Redis configured but down | 503 |

Nothing admits on error. Without Redis, replay protection is per worker, as for
preimages: multi-worker deployments should run Redis.

`nano_enabled` in the [introspection document](./info.md) shows whether the
offer is active, and `l402_payments_nano_total` counts accepted Nano payments.

---

## Notes

- A send of any amount other than the challenge amount, including more, pays
  nothing. Wallets that open the `nano:` URI fill in the amount for the payer.
- A payment made after its challenge expired cannot be redeemed; keep the TTL
  above the time your clients need to pay and retry.
