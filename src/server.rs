use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::storage::Storage;
use crate::torrent::Torrent;

const BLOCK_SIZE: usize = 16 * 1024;
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const MAX_INBOUND_PEERS: usize = 64;

#[derive(Clone)]
pub struct PeerState {
    pub storage: Arc<Mutex<Storage>>,
    pub downloaded: Arc<Mutex<Vec<bool>>>,
    pub uploaded: Arc<AtomicU64>,
    have_senders: Arc<Mutex<Vec<Sender<usize>>>>,
}

impl PeerState {
    pub fn new(storage: Storage, piece_count: usize) -> Self {
        Self {
            storage: Arc::new(Mutex::new(storage)),
            downloaded: Arc::new(Mutex::new(vec![false; piece_count])),
            uploaded: Arc::new(AtomicU64::new(0)),
            have_senders: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn mark_piece_available(&self, index: usize) -> Result<()> {
        let mut downloaded = self
            .downloaded
            .lock()
            .map_err(|_| anyhow::anyhow!("piece availability lock was poisoned"))?;
        let piece = downloaded
            .get_mut(index)
            .context("piece index is out of range")?;
        *piece = true;
        drop(downloaded);

        let mut senders = self
            .have_senders
            .lock()
            .map_err(|_| anyhow::anyhow!("peer notification lock was poisoned"))?;
        senders.retain(|sender| sender.send(index).is_ok());
        Ok(())
    }
}

pub struct PeerServer {
    address: SocketAddr,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PeerServer {
    pub fn start(
        bind_address: SocketAddr,
        torrent: Arc<Torrent>,
        peer_id: [u8; 20],
        state: PeerState,
    ) -> Result<Self> {
        let listener = TcpListener::bind(bind_address)
            .with_context(|| format!("could not listen for peers on {bind_address}"))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let active_peers = Arc::new(AtomicUsize::new(0));
        let thread = thread::Builder::new()
            .name("manta-peer-listener".to_owned())
            .spawn(move || {
                while thread_running.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, peer_address)) => {
                            if active_peers.fetch_add(1, Ordering::AcqRel) >= MAX_INBOUND_PEERS {
                                active_peers.fetch_sub(1, Ordering::AcqRel);
                                drop(stream);
                                continue;
                            }
                            let torrent = Arc::clone(&torrent);
                            let state = state.clone();
                            let peer_running = Arc::clone(&thread_running);
                            let peer_count = Arc::clone(&active_peers);
                            thread::spawn(move || {
                                let _peer_guard = ActivePeerGuard(peer_count);
                                if let Err(error) = serve_peer(
                                    stream,
                                    peer_address,
                                    &torrent,
                                    &peer_id,
                                    &state,
                                    &peer_running,
                                ) {
                                    eprintln!("Inbound peer {peer_address}: {error:#}");
                                }
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(100));
                        }
                        Err(error) => {
                            eprintln!("Peer listener accept failed: {error}");
                            thread::sleep(Duration::from_secs(1));
                        }
                    }
                }
            })
            .context("could not start peer listener thread")?;

        Ok(Self {
            address,
            running,
            thread: Some(thread),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn shutdown(mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("peer listener thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for PeerServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

struct ActivePeerGuard(Arc<AtomicUsize>);

impl Drop for ActivePeerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn serve_peer(
    mut stream: TcpStream,
    peer_address: SocketAddr,
    torrent: &Torrent,
    peer_id: &[u8; 20],
    state: &PeerState,
    running: &AtomicBool,
) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;

    let mut handshake = [0; 68];
    stream
        .read_exact(&mut handshake)
        .context("could not read peer handshake")?;
    if handshake[0] != 19 || &handshake[1..20] != b"BitTorrent protocol" {
        bail!("peer sent an invalid BitTorrent handshake");
    }
    if handshake[28..48] != torrent.info_hash {
        bail!("peer requested a different torrent");
    }

    let mut response = Vec::with_capacity(68);
    response.push(19);
    response.extend_from_slice(b"BitTorrent protocol");
    response.extend_from_slice(&[0; 8]);
    response.extend_from_slice(&torrent.info_hash);
    response.extend_from_slice(peer_id);
    stream.write_all(&response)?;

    let (have_sender, have_receiver) = mpsc::channel();
    state
        .have_senders
        .lock()
        .map_err(|_| anyhow::anyhow!("peer notification lock was poisoned"))?
        .push(have_sender);
    send_bitfield(&mut stream, state)?;

    while running.load(Ordering::Relaxed) {
        while let Ok(index) = have_receiver.try_recv() {
            send_message(&mut stream, 4, &(index as u32).to_be_bytes())?;
        }

        match read_message(&mut stream) {
            Ok(None) => continue,
            Ok(Some((2, _))) => send_message(&mut stream, 1, &[])?,
            Ok(Some((6, payload))) => {
                serve_request(&mut stream, peer_address, payload, torrent, state)?;
            }
            Ok(Some(_)) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn send_bitfield(stream: &mut TcpStream, state: &PeerState) -> Result<()> {
    let downloaded = state
        .downloaded
        .lock()
        .map_err(|_| anyhow::anyhow!("piece availability lock was poisoned"))?;
    if downloaded.is_empty() {
        return Ok(());
    }
    let mut bitfield = vec![0; downloaded.len().div_ceil(8)];
    for (index, available) in downloaded.iter().enumerate() {
        if *available {
            bitfield[index / 8] |= 0x80 >> (index % 8);
        }
    }
    send_message(stream, 5, &bitfield)
}

fn serve_request(
    stream: &mut TcpStream,
    peer_address: SocketAddr,
    payload: Vec<u8>,
    torrent: &Torrent,
    state: &PeerState,
) -> Result<()> {
    if payload.len() != 12 {
        bail!("peer {peer_address} sent a malformed piece request");
    }
    let index = u32::from_be_bytes(payload[0..4].try_into().expect("4 bytes")) as usize;
    let begin = u32::from_be_bytes(payload[4..8].try_into().expect("4 bytes")) as usize;
    let length = u32::from_be_bytes(payload[8..12].try_into().expect("4 bytes")) as usize;
    if length == 0 || length > BLOCK_SIZE {
        bail!("peer {peer_address} requested an invalid block size");
    }
    let available = state
        .downloaded
        .lock()
        .map_err(|_| anyhow::anyhow!("piece availability lock was poisoned"))?
        .get(index)
        .copied()
        .unwrap_or(false);
    if !available {
        return Ok(());
    }

    let piece_offset = index
        .checked_mul(torrent.piece_length)
        .context("requested piece offset overflow")?;
    let total_length =
        usize::try_from(torrent.total_length).context("torrent is too large for this platform")?;
    let piece_length = total_length
        .checked_sub(piece_offset)
        .context("peer requested a piece outside the torrent")?
        .min(torrent.piece_length);
    let block_end = begin
        .checked_add(length)
        .context("requested block end overflow")?;
    if block_end > piece_length {
        bail!("peer {peer_address} requested a block outside the piece");
    }
    let absolute_offset = piece_offset
        .checked_add(begin)
        .context("requested block offset overflow")?;
    let block = state
        .storage
        .lock()
        .map_err(|_| anyhow::anyhow!("torrent storage lock was poisoned"))?
        .read_range(absolute_offset as u64, length)?;

    let mut piece_payload = Vec::with_capacity(8 + block.len());
    piece_payload.extend_from_slice(&(index as u32).to_be_bytes());
    piece_payload.extend_from_slice(&(begin as u32).to_be_bytes());
    piece_payload.extend_from_slice(&block);
    send_message(stream, 7, &piece_payload)?;
    state
        .uploaded
        .fetch_add(block.len() as u64, Ordering::Relaxed);
    Ok(())
}

fn read_message(stream: &mut TcpStream) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 {
        return Ok(None);
    }
    if length > MAX_MESSAGE_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "peer message exceeds the 1 MiB safety limit",
        ));
    }
    let mut message = vec![0; length];
    stream.read_exact(&mut message)?;
    Ok(Some((message[0], message[1..].to_vec())))
}

fn send_message(stream: &mut TcpStream, id: u8, payload: &[u8]) -> Result<()> {
    let length = u32::try_from(payload.len() + 1).context("peer message is too large")?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(&[id])?;
    stream.write_all(payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use sha1::{Digest, Sha1};

    use super::*;
    use crate::torrent::TorrentFile;

    #[test]
    fn accepts_peer_handshake_and_serves_verified_piece() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let destination = std::env::temp_dir().join(format!("manta-peer-test-{unique}"));
        let bytes = b"test";
        let torrent = Arc::new(Torrent {
            name: "test.bin".to_owned(),
            files: vec![TorrentFile {
                path: PathBuf::from("test.bin"),
                length: bytes.len() as u64,
            }],
            piece_length: bytes.len(),
            piece_hashes: vec![Sha1::digest(bytes).into()],
            info_hash: [7; 20],
            announce_urls: Vec::new(),
            total_length: bytes.len() as u64,
        });
        let mut storage = Storage::create(&torrent, &destination).unwrap();
        storage.write_piece(0, torrent.piece_length, bytes).unwrap();
        let state = PeerState::new(storage, 1);
        state.mark_piece_available(0).unwrap();
        let peer_id = *b"-MT0001-000000000000";
        let server = PeerServer::start(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            Arc::clone(&torrent),
            peer_id,
            state,
        )
        .unwrap();

        let mut stream = TcpStream::connect(server.address()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut handshake = Vec::with_capacity(68);
        handshake.push(19);
        handshake.extend_from_slice(b"BitTorrent protocol");
        handshake.extend_from_slice(&[0; 8]);
        handshake.extend_from_slice(&torrent.info_hash);
        handshake.extend_from_slice(b"-TEST00-000000000000");
        stream.write_all(&handshake).unwrap();

        let mut response = [0; 68];
        stream.read_exact(&mut response).unwrap();
        assert_eq!(response[28..48], torrent.info_hash);
        assert_eq!(&response[48..68], &peer_id);
        assert_eq!(read_message(&mut stream), (5, vec![0x80]));

        thread::sleep(Duration::from_millis(600));
        write_message(&mut stream, 2, &[]);
        assert_eq!(read_message(&mut stream), (1, vec![]));
        let mut request = Vec::new();
        request.extend_from_slice(&0_u32.to_be_bytes());
        request.extend_from_slice(&0_u32.to_be_bytes());
        request.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        write_message(&mut stream, 6, &request);
        assert_eq!(
            read_message(&mut stream),
            (7, [vec![0; 8], bytes.to_vec()].concat())
        );

        drop(stream);
        server.shutdown().unwrap();
        std::fs::remove_dir_all(destination).unwrap();
    }

    fn write_message(stream: &mut TcpStream, id: u8, payload: &[u8]) {
        stream
            .write_all(&((payload.len() + 1) as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&[id]).unwrap();
        stream.write_all(payload).unwrap();
    }

    fn read_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut length = [0; 4];
        stream.read_exact(&mut length).unwrap();
        let length = u32::from_be_bytes(length) as usize;
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).unwrap();
        (payload.remove(0), payload)
    }
}
