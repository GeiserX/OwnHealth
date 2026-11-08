"""REST routers. Every module here that defines `router` (a FastAPI APIRouter) is mounted under
`/api` by `ownhealth.app.create_app`. One file per lane area; see CONTRACTS.md for ownership.
Modules whose name starts with `_` are helpers and never mounted.
"""
