# One image for the MCP server and the paying client: both only need the
# MCP Python SDK, which speaks the 2026-07-28 Streamable HTTP transport.
FROM python:3.12-slim
# Print each line as it happens, in order, even when the output is piped.
ENV PYTHONUNBUFFERED=1
# client.py also imports httpx2, the HTTP client the SDK is built on.
RUN pip install --no-cache-dir "mcp==2.3.0" "httpx2==2.16.2"
WORKDIR /app
COPY server.py client.py ./
