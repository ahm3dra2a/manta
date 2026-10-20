# Manta

Manta is an experimental BitTorrent v1 client written in Rust. Its transfer
engine is also exposed as a library, keeping it reusable by the CLI and the
planned native Windows and Linux desktop applications.

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

During downloads, Manta connects to multiple tracker peers concurrently,
requests multiple blocks per peer at a time, and prioritizes the least commonly
available pieces among connected peers.

Downloads support single- and multi-file torrents. On startup, Manta rechecks
existing output data against the torrent's piece hashes and resumes verified
pieces; incomplete or corrupted pieces are downloaded again. Existing output
files must have their expected sizes. Manta refuses to resize mismatched files
and rejects symbolic links at torrent output paths. Rechecking reads the
existing data on every start; no separate resume metadata file is required.

## Current scope

Manta currently supports `.torrent` files, BitTorrent v1, HTTP(S) trackers, and
the TCP peer protocol. UDP trackers, magnet links, DHT, peer exchange, local
peer discovery, µTP, webseeds, BitTorrent v2, fast-resume metadata, and
multi-torrent management are not implemented.

Manta is dual-licensed under MIT or Apache-2.0; see [LICENSE-MIT](LICENSE-MIT)
and [LICENSE-APACHE](LICENSE-APACHE).
