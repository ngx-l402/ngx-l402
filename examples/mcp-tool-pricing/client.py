"""An MCP client that pays for each tool call over Lightning.

It is the stock MCP Python SDK client with one addition: an HTTP auth hook
that answers a 402 by paying the invoice and retrying the same request with
`Authorization: L402 <macaroon>:<preimage>`. The SDK still builds every
request, including the `Mcp-Method` / `Mcp-Name` headers ngx-l402 prices by.

    docker compose run --rm client
"""

import asyncio
import base64
import os
import re
import sys

import httpx2
from mcp import Client
from mcp.client.streamable_http import streamable_http_client


def env(name: str) -> str:
    """docker-compose.yml sets these; outside compose, point them at your own."""
    value = os.environ.get(name)
    if not value:
        sys.exit(f"{name} is not set; run with `docker compose run --rm client`")
    return value


# The most the client pays for one call. A server can put any amount in its
# invoice, so anything above this is refused instead of paid.
MAX_SATS = int(os.environ.get("MAX_SATS", "100"))


class LndWallet:
    """Pays BOLT11 invoices through an LND node's REST API.

    Uses the node's admin macaroon, which can spend: fine for this regtest
    wallet, but give a real wallet a macaroon limited to what it needs.
    """

    def __init__(self, rest_url: str, lnd_dir: str) -> None:
        macaroon = f"{lnd_dir}/data/chain/bitcoin/regtest/admin.macaroon"
        with open(macaroon, "rb") as f:
            headers = {"Grpc-Metadata-macaroon": f.read().hex()}
        self.http = httpx2.AsyncClient(
            base_url=rest_url, headers=headers, verify=f"{lnd_dir}/tls.cert", timeout=60
        )
        self.spent_sat = 0

    async def amount_sat(self, invoice: str) -> int:
        r = await self.http.get(f"/v1/payreq/{invoice}")
        r.raise_for_status()
        return int(r.json()["num_satoshis"])

    async def pay(self, invoice: str) -> str:
        """Pay `invoice` and return the preimage as hex."""
        r = await self.http.post("/v1/channels/transactions", json={"payment_request": invoice})
        r.raise_for_status()
        body = r.json()
        if body.get("payment_error"):
            raise RuntimeError(f"payment failed: {body['payment_error']}")
        return base64.b64decode(body["payment_preimage"]).hex()


class PaymentRefused(Exception):
    """The server asked for more than MAX_SATS."""


class L402Auth(httpx2.Auth):
    """On a 402, pay the invoice and send the request again with the token."""

    def __init__(self, wallet: LndWallet) -> None:
        self.wallet = wallet

    async def async_auth_flow(self, request):
        response = yield request
        if response.status_code != 402:
            return
        challenge = response.headers.get("www-authenticate", "")
        macaroon = re.search(r'macaroon="([^"]+)"', challenge)
        invoice = re.search(r'invoice="([^"]+)"', challenge)
        if not (macaroon and invoice):
            return
        tool = request.headers.get("mcp-name", "?")
        sats = await self.wallet.amount_sat(invoice.group(1))
        if sats > MAX_SATS:
            raise PaymentRefused(f"{tool} costs {sats} sats, more than MAX_SATS={MAX_SATS}")
        print(f"  402 for {tool}: paying {sats} sats")
        preimage = await self.wallet.pay(invoice.group(1))
        self.wallet.spent_sat += sats
        request.headers["Authorization"] = f"L402 {macaroon.group(1)}:{preimage}"
        yield request


async def main() -> int:
    mcp_url = env("MCP_URL")
    wallet = LndWallet(env("LND_REST_URL"), env("LND_DIR"))
    # The SDK's own timeouts: a tool's response stream may stay open a while.
    http = httpx2.AsyncClient(auth=L402Auth(wallet), timeout=httpx2.Timeout(30, read=300))
    async with Client(streamable_http_client(mcp_url, http_client=http)) as client:
        print(f"Connected to {mcp_url} (protocol {client.protocol_version})")

        tools = await client.list_tools()
        print("Tools (listing is free):", ", ".join(t.name for t in tools.tools))

        for name, args in [
            ("search", {"query": "lightning"}),
            ("summarize", {"text": "MCP servers can charge per tool call when ngx-l402 sits in front of them"}),
        ]:
            print(f"Calling {name}")
            result = await client.call_tool(name, args)
            if result.is_error:
                print(f"  error: {result.content}")
                return 1
            print(f"  -> {result.content[0].text}")

    print(f"Spent {wallet.spent_sat} sats")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except* PaymentRefused as group:
        sys.exit(f"Not paying: {group.exceptions[0]}")
