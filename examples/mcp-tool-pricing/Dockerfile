# One image for the MCP server and the paying client: both only need the
# MCP Python SDK, which speaks the 2026-07-28 Streamable HTTP transport.
FROM python:3.12-slim
RUN pip install --no-cache-dir "mcp==2.3.0"
WORKDIR /app
COPY server.py client.py ./
