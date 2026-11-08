"""Per-lane demo hooks. One module per lane: `ORDER = <int>` and `def seed(conn, ctx) -> None`.

Suggested ORDER bands: 100 normalize (A), 200 lab-pdf (B), 300 apple/fhir/manual (C), 400 genome (D),
500 rules/status (E), 600 mcp (F), 700 security (G), 900 ops (J).
"""
