# Manta

Manta is an experimental BitTorrent v1 command-line client written in Rust.

## Build and run

```powershell
cargo run -- inspect path\to\file.torrent
cargo run -- download path\to\file.torrent [destination] [--port 51413]
```

`inspect` prints torrent metadata and its v1 info hash. `download` contacts
HTTP(S) trackers, accepts inbound TCP peer connections, downloads and verifies
pieces, and serves verified pieces to connected peers. The default peer port is
`51413`; specify `--port 0` to let the operating system choose an available
port. Manta announces the bound port to trackers and sends `started`,
`completed`, and `stopped` events. It continues seeding by serving verified
pieces after the download is complete until stopped with Ctrl+C. Inbound
connections are limited to 64 peers. Public reachability depends on the host
firewall and network allowing connections to the selected peer port.

Downloads support single- and multi-file torrents and do not overwrite
existing output files. Verified pieces are kept in memory as progress state for
the current run, but are not yet resumed after restarting Manta.

## Current scope

Manta currently supports `.torrent` files, BitTorrent v1, HTTP(S) trackers, and
the TCP peer protocol. UDP trackers, magnet links, DHT, peer exchange, local
peer discovery, µTP, webseeds, BitTorrent v2, persistent resume data, and
parallel block requests are not implemented.

Manta is dual-licensed under MIT or Apache-2.0; see [LICENSE-MIT](LICENSE-MIT)
and [LICENSE-APACHE](LICENSE-APACHE).
