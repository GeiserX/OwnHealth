# Connector vendor fixtures

One folder per US vendor in `data/connectors/connectors.yaml`. The connector tests serve them from
the fake vendor (`server/crates/ownhealth-core/tests/connector_fake`), so each vendor's connect and
refresh run against what that vendor publishes.

- `smart-configuration.json` is the vendor's real SMART discovery document, recorded on 2026-11-25
  from a public server of that vendor with a plain GET (no account, no token, no personal data).
  `scopes_supported` is cut to the scopes OwnHealth asks for; every other field is as the vendor sent
  it. The fake replaces the host of each endpoint with its own loopback address.

  | Vendor | Recorded from |
  |---|---|
  | epic | `https://fhir.epic.com/interconnect-fhir-oauth/api/FHIR/R4` (Epic's sandbox) |
  | oracle | `https://fhir-myrecord.cerner.com/r4/ec2458f2-1e24-41c8-b71b-0e701af7583d` (Oracle's patient sandbox) |
  | athena | `https://api.platform.athenahealth.com/fhir/r4` |
  | healow | `https://fhir4.healow.com/fhir/r4/DEBJCD` (the first practice in healow's endpoint list) |
  | nextgen | `https://fhir.nextgen.com/nge/prod/fhir-api-r4/fhir/r4` |
  | meditech | `https://greenfield-prod-apis.meditech.com/v2/uscore/STU6` (MEDITECH's Greenfield server) |
  | va | `https://api.va.gov/services/fhir/v0/r4` |

- `tokens.json` holds the answers of the token endpoint: `code` for the sign-in, `refresh` for a
  refresh. These are not recordings: getting one needs an account at the vendor. They follow SMART App
  Launch and the vendor's profile in `connectors.yaml` (a vendor whose refresh tokens rotate sends a
  new `refresh_token`; Oracle's do not rotate and its access tokens last 570 seconds). The fake fills
  in `$ACCESS`, `$REFRESH` and `$PATIENT` with synthetic values.

No vendor here is verified live: `verified_live` stays false until a real account connects.
