use std::collections::HashSet;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::bencode;
use crate::torrent::Torrent;

const DEFAULT_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30 * 60);
const MIN_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct AnnounceResult {
    pub peers: Vec<SocketAddr>,
    pub interval: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnounceEvent {
    Started,
    Completed,
    Stopped,
}

impl AnnounceEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Stopped => "stopped",
        }
    }
}

pub struct AnnounceRequest {
    pub port: u16,
    pub downloaded: u64,
    pub left: u64,
    pub uploaded: u64,
    pub event: Option<AnnounceEvent>,
}

pub fn announce(
    torrent: &Torrent,
    peer_id: &[u8; 20],
    request: &AnnounceRequest,
) -> Result<AnnounceResult> {
    let mut errors = Vec::new();
    let mut peers = Vec::new();
    let mut unique_peers = HashSet::new();
    let mut interval = None;
    let mut successful_trackers = 0;

    for tracker in &torrent.announce_urls {
        match announce_to(tracker, torrent, peer_id, request) {
            Ok(result) => {
                successful_trackers += 1;
                interval = Some(interval.map_or(result.interval, |current: Duration| {
                    current.min(result.interval)
                }));
                for peer in result.peers {
                    if unique_peers.insert(peer) {
                        peers.push(peer);
                    }
                }
            }
            Err(error) => {
                let message = format!("{tracker}: {error:#}");
                eprintln!("Tracker unavailable: {message}");
                errors.push(message);
            }
        }
    }
    if successful_trackers == 0 {
        bail!("all trackers failed: {}", errors.join("; "))
    }
    Ok(AnnounceResult {
        peers,
        interval: interval.unwrap_or(DEFAULT_ANNOUNCE_INTERVAL),
    })
}

fn announce_to(
    tracker: &str,
    torrent: &Torrent,
    peer_id: &[u8; 20],
    request: &AnnounceRequest,
) -> Result<AnnounceResult> {
    let url = build_announce_url(tracker, torrent, peer_id, request);
    let response = ureq::get(&url)
        .timeout(Duration::from_secs(25))
        .call()
        .map_err(|error| match error {
        ureq::Error::Status(status, response) => {
            let mut body = Vec::new();
            let read_result = response.into_reader().take(2048).read_to_end(&mut body);
            match read_result {
                Ok(_) if body.is_empty() => anyhow::anyhow!("tracker returned HTTP {status}"),
                Ok(_) => anyhow::anyhow!(
                    "tracker returned HTTP {status}: {}",
                    String::from_utf8_lossy(&body).trim()
                ),
                Err(read_error) => anyhow::anyhow!(
                    "tracker returned HTTP {status}; could not read error response: {read_error}"
                ),
            }
        }
        other => anyhow::anyhow!("tracker request failed: {other}"),
    })?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(4 * 1024 * 1024)
        .read_to_end(&mut body)
        .context("could not read tracker response")?;
    parse_announce_response(&body)
}

fn build_announce_url(
    tracker: &str,
    torrent: &Torrent,
    peer_id: &[u8; 20],
    request: &AnnounceRequest,
) -> String {
    let separator = if tracker.contains('?') { '&' } else { '?' };
    let mut url = format!(
        "{tracker}{separator}info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1",
        percent_encode(&torrent.info_hash),
        percent_encode(peer_id),
        request.port,
        request.uploaded,
        request.downloaded,
        request.left,
    );
    if let Some(event) = request.event {
        use std::fmt::Write;
        write!(&mut url, "&event={}", event.as_str()).expect("writing to a string cannot fail");
    }
    url
}

fn parse_announce_response(body: &[u8]) -> Result<AnnounceResult> {
    let response = bencode::parse_with_trailing_whitespace(body)
        .context("tracker returned invalid bencode")?
        .value;
    if let Some(failure) = response.get(b"failure reason") {
        bail!(
            "tracker failure: {}",
            String::from_utf8_lossy(failure.as_bytes()?)
        );
    }
    let interval = response
        .get(b"interval")
        .map(|value| value.as_integer())
        .transpose()?
        .filter(|seconds| *seconds > 0)
        .map(|seconds| {
            Duration::from_secs(seconds as u64)
                .max(MIN_ANNOUNCE_INTERVAL)
                .min(DEFAULT_ANNOUNCE_INTERVAL)
        })
        .unwrap_or(DEFAULT_ANNOUNCE_INTERVAL);
    let mut peers = Vec::new();
    if let Some(compact) = response.get(b"peers") {
        match compact {
            bencode::Value::Bytes(bytes) => peers.extend(parse_compact_peers(bytes)?),
            bencode::Value::List(entries) => {
                peers.extend(entries.iter().map(parse_peer).collect::<Result<Vec<_>>>()?)
            }
            _ => bail!("tracker peers field has an unsupported format"),
        }
    }
    if let Some(compact) = response.get(b"peers6") {
        peers.extend(parse_compact_ipv6_peers(
            compact
                .as_bytes()
                .context("tracker peers6 field must be bytes")?,
        )?);
    }
    Ok(AnnounceResult { peers, interval })
}

fn parse_compact_peers(bytes: &[u8]) -> Result<Vec<SocketAddr>> {
    if !bytes.len().is_multiple_of(6) {
        bail!("compact IPv4 peer list has an invalid length");
    }
    Ok(bytes
        .chunks(6)
        .map(|peer| {
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(peer[0], peer[1], peer[2], peer[3])),
                u16::from_be_bytes([peer[4], peer[5]]),
            )
        })
        .filter(|peer| peer.port() != 0)
        .collect())
}

fn parse_compact_ipv6_peers(bytes: &[u8]) -> Result<Vec<SocketAddr>> {
    if !bytes.len().is_multiple_of(18) {
        bail!("compact IPv6 peer list has an invalid length");
    }
    Ok(bytes
        .chunks(18)
        .map(|peer| {
            let address: [u8; 16] = peer[..16].try_into().expect("16-byte address");
            SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(address)),
                u16::from_be_bytes([peer[16], peer[17]]),
            )
        })
        .filter(|peer| peer.port() != 0)
        .collect())
}

fn parse_peer(peer: &bencode::Value) -> Result<SocketAddr> {
    let ip = peer
        .get(b"ip")
        .context("tracker peer has no IP")?
        .as_bytes()
        .context("tracker peer IP must be bytes")?;
    let ip = std::str::from_utf8(ip).context("tracker peer IP is not UTF-8")?;
    let port = peer
        .get(b"port")
        .context("tracker peer has no port")?
        .as_integer()
        .context("tracker peer port must be an integer")?;
    let port = u16::try_from(port).context("tracker peer port is out of range")?;
    if port == 0 {
        bail!("tracker peer port cannot be zero");
    }
    let ip = ip.parse().or_else(|_| {
        if let Some(hex) = ip.strip_prefix("v6:") {
            let bytes = decode_hex(hex)?;
            let octets: [u8; 16] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid compact IPv6 address"))?;
            Ok(IpAddr::V6(Ipv6Addr::from(octets)))
        } else {
            Err(anyhow::anyhow!("invalid peer IP address"))
        }
    })?;
    Ok(SocketAddr::new(ip, port))
}

fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        bail!("invalid hexadecimal IPv6 address");
    }
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair)?;
            u8::from_str_radix(text, 16).context("invalid hexadecimal IPv6 address")
        })
        .collect()
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 3);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut encoded, "%{byte:02X}").expect("writing to a string cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_compact_peers() {
        let peers = parse_compact_peers(&[127, 0, 0, 1, 0x1a, 0xe1]).unwrap();
        assert_eq!(peers, vec!["127.0.0.1:6881".parse().unwrap()]);
        assert!(parse_compact_peers(&[1, 2]).is_err());
        let mut compact_ipv6 = [0; 18];
        compact_ipv6[16..].copy_from_slice(&[0x1a, 0xe1]);
        let ipv6 = parse_compact_ipv6_peers(&compact_ipv6).unwrap();
        assert_eq!(ipv6, vec!["[::]:6881".parse().unwrap()]);
        assert!(parse_compact_ipv6_peers(&[1, 2]).is_err());
    }

    #[test]
    fn encodes_binary_query_parameters() {
        assert_eq!(percent_encode(&[0, 0xff, b'a']), "%00%FF%61");
    }

    #[test]
    fn builds_announce_urls_with_real_port_counters_and_lifecycle_events() {
        let torrent = Torrent {
            name: "test".to_owned(),
            files: Vec::new(),
            piece_length: 1,
            piece_hashes: Vec::new(),
            info_hash: [0; 20],
            announce_urls: Vec::new(),
            total_length: 0,
        };
        let request = AnnounceRequest {
            port: 51413,
            downloaded: 123,
            left: 456,
            uploaded: 789,
            event: Some(AnnounceEvent::Started),
        };
        let url = build_announce_url(
            "https://tracker.example/announce?passkey=x",
            &torrent,
            &[0; 20],
            &request,
        );
        assert!(url.contains("&port=51413"));
        assert!(url.contains("&uploaded=789"));
        assert!(url.contains("&downloaded=123"));
        assert!(url.contains("&left=456"));
        assert!(url.ends_with("&event=started"));
    }

    #[test]
    fn parses_empty_peer_response_and_bounds_announce_interval() {
        let response = parse_announce_response(b"d8:intervali1e5:peers0:e\r\n").unwrap();
        assert!(response.peers.is_empty());
        assert_eq!(response.interval, MIN_ANNOUNCE_INTERVAL);

        let response = parse_announce_response(b"d8:intervali99999999e5:peers0:e").unwrap();
        assert_eq!(response.interval, DEFAULT_ANNOUNCE_INTERVAL);
    }

    #[test]
    fn reports_tracker_failure_reason() {
        let error = parse_announce_response(b"d14:failure reason4:gonee").unwrap_err();
        assert!(error.to_string().contains("tracker failure: gone"));
    }
}
