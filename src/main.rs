mod bencode;
mod peer;
mod server;
mod storage;
mod torrent;
mod tracker;

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

const DEFAULT_PEER_PORT: u16 = 51413;

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("inspect") if args.len() == 2 => inspect(Path::new(&args[1])),
        Some("download") if args.len() >= 2 => download_command(&args[1..]),
        _ => {
            print_usage();
            bail!("invalid command-line arguments")
        }
    }
}

fn download_command(args: &[String]) -> Result<()> {
    let torrent_path = Path::new(&args[0]);
    let (destination, port) = parse_download_options(&args[1..])?;
    download(torrent_path, &destination, port)
}

fn parse_download_options(args: &[String]) -> Result<(PathBuf, u16)> {
    let mut destination = None;
    let mut port = DEFAULT_PEER_PORT;
    let mut index = 0;

    while index < args.len() {
        if args[index] == "--port" {
            index += 1;
            let value = args.get(index).context("--port requires a port number")?;
            port = value
                .parse()
                .context("peer port must be between 0 and 65535")?;
        } else if let Some(value) = args[index].strip_prefix("--port=") {
            port = value
                .parse()
                .context("peer port must be between 0 and 65535")?;
        } else if args[index].starts_with('-') {
            bail!("unknown download option: {}", args[index]);
        } else if destination.replace(PathBuf::from(&args[index])).is_some() {
            bail!("download accepts only one destination directory");
        }
        index += 1;
    }

    Ok((destination.unwrap_or_else(|| PathBuf::from(".")), port))
}

fn inspect(path: &Path) -> Result<()> {
    let torrent = read_torrent(path)?;
    println!("Name: {}", torrent.name);
    println!("Size: {} bytes", torrent.total_length);
    println!("Files: {}", torrent.files.len());
    println!("Piece length: {} bytes", torrent.piece_length);
    println!("Pieces: {}", torrent.piece_hashes.len());
    println!("Info hash: {}", hex(&torrent.info_hash));
    println!("Trackers:");
    for tracker in &torrent.announce_urls {
        println!("  {tracker}");
    }
    Ok(())
}

fn download(path: &Path, destination: &Path, requested_port: u16) -> Result<()> {
    let torrent = Arc::new(read_torrent(path)?);
    println!(
        "Downloading {} ({} bytes)",
        torrent.name, torrent.total_length
    );
    let peer_id = make_peer_id();
    let storage = storage::Storage::create(&torrent, destination)?;
    let peer_state = server::PeerState::new(storage, torrent.piece_hashes.len());
    let server = server::PeerServer::start(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), requested_port),
        Arc::clone(&torrent),
        peer_id,
        peer_state.clone(),
    )?;
    println!("Listening for peers on {}", server.address());

    let running = Arc::new(AtomicBool::new(true));
    let signal_state = Arc::clone(&running);
    ctrlc::set_handler(move || signal_state.store(false, Ordering::Relaxed))
        .context("could not register Ctrl+C handler")?;

    let downloaded_bytes = Arc::new(AtomicU64::new(0));
    let mut retry_delay = Duration::from_secs(15);
    let mut tracker_started = false;
    let mut completion_announced = false;

    while running.load(Ordering::Relaxed) {
        let downloaded = peer_state
            .downloaded
            .lock()
            .map_err(|_| anyhow::anyhow!("piece availability lock was poisoned"))?
            .iter()
            .all(|piece| *piece);
        if downloaded && tracker_started && !completion_announced {
            let request = tracker_request(
                server.address().port(),
                downloaded_bytes.load(Ordering::Relaxed),
                0,
                peer_state.uploaded.load(Ordering::Relaxed),
                Some(tracker::AnnounceEvent::Completed),
            );
            match tracker::announce(&torrent, &peer_id, &request) {
                Ok(_) => {
                    completion_announced = true;
                    println!("Download complete. Seeding until Ctrl+C.");
                    retry_delay = Duration::from_secs(15);
                }
                Err(error) => {
                    eprintln!("Could not announce completion: {error:#}");
                    sleep_interruptible(retry_delay, &running);
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(15 * 60));
                    continue;
                }
            }
        }

        if !running.load(Ordering::Relaxed) {
            break;
        }

        let left = if completion_announced {
            0
        } else {
            torrent
                .total_length
                .saturating_sub(downloaded_bytes.load(Ordering::Relaxed))
        };
        let request = tracker_request(
            server.address().port(),
            downloaded_bytes.load(Ordering::Relaxed),
            left,
            peer_state.uploaded.load(Ordering::Relaxed),
            (!tracker_started).then_some(tracker::AnnounceEvent::Started),
        );
        let announced = match tracker::announce(&torrent, &peer_id, &request) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("No tracker responded: {error:#}");
                eprintln!(
                    "Retrying in {} seconds (Ctrl+C to stop)...",
                    retry_delay.as_secs()
                );
                sleep_interruptible(retry_delay, &running);
                retry_delay = (retry_delay * 2).min(Duration::from_secs(15 * 60));
                continue;
            }
        };
        tracker_started = true;
        retry_delay = Duration::from_secs(15);

        if completion_announced {
            let delay = announced.interval.max(Duration::from_secs(30));
            eprintln!(
                "Seeding; next tracker announce in {} seconds.",
                delay.as_secs()
            );
            sleep_interruptible(delay, &running);
            continue;
        }

        if announced.peers.is_empty() {
            eprintln!(
                "Trackers responded but returned no peers. Retrying in {} seconds (Ctrl+C to stop)...",
                announced.interval.max(Duration::from_secs(30)).as_secs()
            );
            sleep_interruptible(announced.interval.max(Duration::from_secs(30)), &running);
            continue;
        }

        println!("Tracker(s) returned {} peer(s)", announced.peers.len());
        let have_todo = peer_state
            .downloaded
            .lock()
            .map_err(|_| anyhow::anyhow!("piece availability lock was poisoned"))?
            .clone();
        let mut downloaded = have_todo;
        let newly_downloaded = peer::download(
            &torrent,
            &announced.peers,
            &peer_id,
            &mut downloaded,
            |index, bytes| {
                peer_state
                    .storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("torrent storage lock was poisoned"))?
                    .write_piece(index, torrent.piece_length, bytes)?;
                peer_state.mark_piece_available(index)?;
                downloaded_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(())
            },
        )?;

        if newly_downloaded == 0 {
            eprintln!("No new pieces downloaded this round.");
        }
        let complete = downloaded.iter().all(|done| *done);
        if !complete {
            let delay = announced.interval.max(Duration::from_secs(30));
            eprintln!(
                "Trying trackers and peers again in {} seconds (Ctrl+C to stop)...",
                delay.as_secs()
            );
            sleep_interruptible(delay, &running);
        }
    }

    let stopped_request = tracker_request(
        server.address().port(),
        downloaded_bytes.load(Ordering::Relaxed),
        torrent
            .total_length
            .saturating_sub(downloaded_bytes.load(Ordering::Relaxed)),
        peer_state.uploaded.load(Ordering::Relaxed),
        Some(tracker::AnnounceEvent::Stopped),
    );
    if let Err(error) = tracker::announce(&torrent, &peer_id, &stopped_request) {
        eprintln!("Could not announce stopped event: {error:#}");
    }
    server.shutdown()?;
    println!("Stopped.");
    Ok(())
}

fn tracker_request(
    port: u16,
    downloaded: u64,
    left: u64,
    uploaded: u64,
    event: Option<tracker::AnnounceEvent>,
) -> tracker::AnnounceRequest {
    tracker::AnnounceRequest {
        port,
        downloaded,
        left,
        uploaded,
        event,
    }
}

fn sleep_interruptible(duration: Duration, running: &AtomicBool) {
    let mut remaining = duration;
    let interval = Duration::from_millis(250);
    while !remaining.is_zero() && running.load(Ordering::Relaxed) {
        let sleep_for = remaining.min(interval);
        thread::sleep(sleep_for);
        remaining = remaining.saturating_sub(sleep_for);
    }
}

fn read_torrent(path: &Path) -> Result<torrent::Torrent> {
    let bytes = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    torrent::Torrent::from_bytes(&bytes)
        .with_context(|| format!("could not parse {}", path.display()))
}

fn make_peer_id() -> [u8; 20] {
    let mut peer_id = *b"-MT0001-000000000000";
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        ^ u128::from(std::process::id());
    let suffix = format!("{stamp:012x}");
    peer_id[8..].copy_from_slice(&suffix.as_bytes()[suffix.len() - 12..]);
    peer_id
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  manta inspect <file.torrent>");
    eprintln!("  manta download <file.torrent> [destination] [--port <port>]");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_download_destination_and_peer_port_options() {
        let args = [
            "test.torrent".to_owned(),
            "downloads".to_owned(),
            "--port".to_owned(),
            "60000".to_owned(),
        ];
        let (destination, port) = parse_download_options(&args[1..]).unwrap();
        assert_eq!(destination, PathBuf::from("downloads"));
        assert_eq!(port, 60000);
    }

    #[test]
    fn rejects_duplicate_destinations_and_invalid_ports() {
        let duplicate_destinations = ["first".to_owned(), "second".to_owned()];
        assert!(parse_download_options(&duplicate_destinations).is_err());
        let invalid_port = ["--port".to_owned(), "not-a-port".to_owned()];
        assert!(parse_download_options(&invalid_port).is_err());
    }
}
