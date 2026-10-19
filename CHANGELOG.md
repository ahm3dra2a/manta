# Changelog

All notable changes to Manta are documented here.

## [0.1.0] - 2026-10-19

### Added

- BitTorrent v1 torrent inspection and HTTP(S) tracker announces.
- Retry with backoff when trackers are unavailable.
- TCP peer listening on a configurable port, advertised using the actual bound
  port.
- Outbound piece downloads with SHA-1 verification.
- Inbound peer handshakes, piece availability, and uploads of verified blocks.
- Tracker `started`, `completed`, and `stopped` lifecycle events.
- Seeding after completion until the process receives Ctrl+C.
- MIT OR Apache-2.0 licensing.

### Limitations

- No persistent resume data, magnet links, UDP trackers, DHT, PEX, LPD, µTP,
  webseeds, BitTorrent v2, parallel block requests, or multi-torrent management.
- Inbound connectivity depends on network and firewall configuration.
