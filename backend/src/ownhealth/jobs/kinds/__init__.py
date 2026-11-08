"""Job handlers, one module per lane (`import_pdf.py`, `apple_health.py`, ...).

Each module registers its kinds with `@ownhealth.jobs.handler("kind")`. `load_handlers()`
imports every module here at startup; nothing else needs to change to add a kind.
"""
