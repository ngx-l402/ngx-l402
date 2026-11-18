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
from mcp.shared._httpx_utils import create_mcp_http_client

MCP_URL = os.environ.get("MCP_URL", "http://localhost:8402/mcp")
LND_REST_URL = os.environ.get("LND_REST_URL", "https://localhost:8080")
LND_DIR = os.environ.get("LND_DIR", os.path.expanduser("~/.lnd"))


class LndWallet:
    """Pays BOLT11 invoices through an LND node's REST API."""

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


class L402Auth(httpx2.Auth):
    """On a 402, pay the invoice and send the request again with the token."""

    requires_response_body = True

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
        print(f"  402 for {tool}: paying {sats} sats")
        preimage = await self.wallet.pay(invoice.group(1))
        self.wallet.spent_sat += sats
        request.headers["Authorization"] = f"L402 {macaroon.group(1)}:{preimage}"
        yield request


async def main() -> int:
    wallet = LndWallet(LND_REST_URL, LND_DIR)
    http = create_mcp_http_client(auth=L402Auth(wallet))
    async with Client(streamable_http_client(MCP_URL, http_client=http)) as client:
        print(f"Connected to {MCP_URL} (protocol {client.protocol_version})")

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
    sys.exit(asyncio.run(main()))
