"""The MCP server mounted at /mcp (streamable HTTP, stateless; docs/design/mcp.md).

Eight tools over personal access tokens, scope-filtered lists, the response envelope with budgets
and cursors, per-token limits, resources and prompts. `create_app` calls `build_server(settings)`
and mounts `server.streamable_http_app(...)` routes at `/mcp`; the PAT and Origin guard sits on
that route.
"""

from __future__ import annotations

from mcp.server.mcpserver import MCPServer

from ownhealth.config import Settings
from ownhealth.mcp.server import INSTRUCTIONS, make_server

__all__ = ["INSTRUCTIONS", "build_server"]


def build_server(settings: Settings) -> MCPServer:
    return make_server()
