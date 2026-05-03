# Changelog

## [0.4.1] - 2026-05-03

- migrate normal remote Nostr sends to Envelope v2 with stable UUIDv7 `msg_id`
- store sender-side remote outbound rows through `insert_message_v2` with Nostr transport metadata
- expose `mycel inbox --json` as agent contract v2 with logical IDs, transport metadata, trust, timestamps, and status fields
- keep ACK honest: local ACK tracking uses logical `msg_id`, while reverse Gift Wrap ACK sending is documented as pending/experimental
- align package, site, README, and changelog metadata for the 0.4.1 contract-hardening release

## [0.4.0] - 2026-04-12

- add the transport-boundary core: router seam, unified ingress pipeline, and persistent `agent_endpoints`
- move local and Nostr receive paths onto shared ingress/materialization, including recipient-side local signature verification
- clean up the legacy sync receive path and make the release baseline pass `cargo test` and `cargo clippy -D warnings`

## [0.3.1] - 2026-03-25

## [0.3.0] - 2026-03-25

## [0.2.2] - 2026-03-24
