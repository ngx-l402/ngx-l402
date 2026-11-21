"""A plain MCP server with two tools.

It knows nothing about payments: ngx-l402 in front of it decides what each
tool call costs (nginx.conf prices `search` at 10 sats and `summarize` at 50). What it must do is reject a request whose `Mcp-Method` /
`Mcp-Name` headers disagree with its body, which the MCP Python SDK does for
every 2026-07-28 request. That check is what `l402_mcp_upstream_validates on`
vouches for in nginx.conf.
"""

from mcp.server import MCPServer
from mcp.server.transport_security import TransportSecuritySettings

mcp = MCPServer("paid-tools")


@mcp.tool()
def search(query: str) -> str:
    """Search the catalogue."""
    return f"3 results for {query!r}: alpha, beta, gamma"


@mcp.tool()
def summarize(text: str) -> str:
    """Summarize a text."""
    words = text.split()
    return " ".join(words[:8]) + (" ..." if len(words) > 8 else "")


if __name__ == "__main__":
    mcp.run(
        transport="streamable-http",
        host="0.0.0.0",
        port=8000,
        # Keep DNS-rebinding protection on, and accept the Host nginx sends
        # when it proxies to `http://mcp-server:8000`. If you change
        # proxy_pass in nginx.conf, change this to match, or every request
        # is refused.
        transport_security=TransportSecuritySettings(allowed_hosts=["mcp-server:8000"]),
    )
