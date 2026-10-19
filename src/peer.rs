use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};

use crate::server::PeerState;
use crate::torrent::Torrent;

const BLOCK_SIZE: usize = 16 * 1024;
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const MAX_IN_FLIGHT_BLOCKS: usize = 8;
const MAX_PEERS_PER_ROUND: usize = 32;

pub fn download(
    torrent: &Torrent,
    peers: &[SocketAddr],
    peer_id: &[u8; 20],
    state: &PeerState,
    save_piece: &(dyn Fn(usize, &[u8]) -> Result<()> + Sync),
) -> Result<usize> {
    let initial_progress = state
        .downloaded
        .lock()
        .map_err(|_| anyhow::anyhow!("piece availability lock was poisoned"))?
        .clone();
    if initial_progress.len() != torrent.piece_hashes.len() {
        bail!("download progress does not match the torrent's piece count");
    }

    let before = initial_progress.iter().filter(|done| **done).count();
    let mut unique_addresses = Vec::new();
    for address in peers {
        if !unique_addresses.contains(address) {
            unique_addresses.push(*address);
            if unique_addresses.len() == MAX_PEERS_PER_ROUND {
                break;
            }
        }
    }
    if unique_addresses.is_empty() {
        return Ok(0);
    }

    let connections = thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        for address in unique_addresses {
            let sender = sender.clone();
            scope.spawn(move || {
                let result = connect(address, torrent, peer_id);
                let _ = sender.send((address, result));
            });
        }
        drop(sender);

        let mut connected = Vec::new();
        for (address, result) in receiver {
            match result {
                Ok(peer) => {
                    eprintln!("Connected to {address}");
                    connected.push((address, peer));
                }
                Err(error) => eprintln!("Could not connect to {address}: {error:#}"),
            }
        }
        connected
    });
    if connections.is_empty() {
        return Ok(0);
    }

    let mut rarity = vec![0; torrent.piece_hashes.len()];
    for (_, peer) in &connections {
        for (index, count) in rarity.iter_mut().enumerate() {
            if peer.might_have(index) {
                *count += 1;
            }
        }
    }

    let scheduler = Arc::new(PieceScheduler::new(initial_progress, rarity));
    let newly_downloaded = Arc::new(Mutex::new(0_usize));
    let worker_result = thread::scope(|scope| {
        let mut workers = Vec::with_capacity(connections.len());
        for (address, mut peer) in connections {
            let scheduler = Arc::clone(&scheduler);
            let newly_downloaded = Arc::clone(&newly_downloaded);
            workers.push(scope.spawn(move || -> Result<()> {
                while let Some(index) = scheduler.claim(|index| peer.might_have(index))? {
                    let result = peer.download_piece(index, torrent);
                    match result {
                        Ok(piece)
                            if Sha1::digest(&piece).as_slice() == torrent.piece_hashes[index] =>
                        {
                            if let Err(error) = save_piece(index, &piece) {
                                scheduler.release(index)?;
                                return Err(error);
                            }
                            if let Err(error) = state.mark_piece_available(index) {
                                scheduler.release(index)?;
                                return Err(error);
                            }
                            scheduler.complete(index)?;
                            let completed = scheduler.completed()?;
                            *newly_downloaded.lock().map_err(|_| {
                                anyhow::anyhow!("download progress counter was poisoned")
                            })? += 1;
                            eprintln!(
                                "Downloaded piece {completed}/{} from {address}",
                                torrent.piece_hashes.len()
                            );
                        }
                        Ok(_) => {
                            scheduler.release(index)?;
                            eprintln!("Peer {address} sent corrupt piece {index}");
                            break;
                        }
                        Err(error) => {
                            scheduler.release(index)?;
                            eprintln!("Peer {address} failed on piece {index}: {error:#}");
                            break;
                        }
                    }
                }
                Ok(())
            }));
        }

        let mut first_error = None;
        for worker in workers {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                Err(_) if first_error.is_none() => {
                    first_error = Some(anyhow::anyhow!("peer download worker panicked"));
                }
                Ok(Err(_)) | Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok::<(), anyhow::Error>(())
    });
    worker_result?;
    let after = scheduler.completed()?;
    if after < before {
        bail!("piece scheduler lost previously downloaded progress");
    }
    Ok(after - before)
}

struct PieceScheduler {
    done: Mutex<Vec<bool>>,
    claimed: Mutex<Vec<bool>>,
    rarity: Vec<usize>,
}

impl PieceScheduler {
    fn new(done: Vec<bool>, rarity: Vec<usize>) -> Self {
        Self {
            claimed: Mutex::new(vec![false; done.len()]),
            done: Mutex::new(done),
            rarity,
        }
    }

    fn claim(&self, has_piece: impl Fn(usize) -> bool) -> Result<Option<usize>> {
        let done = self
            .done
            .lock()
            .map_err(|_| anyhow::anyhow!("piece completion state was poisoned"))?;
        let mut claimed = self
            .claimed
            .lock()
            .map_err(|_| anyhow::anyhow!("piece claim state was poisoned"))?;
        let Some(candidate) = (0..done.len())
            .filter(|index| {
                !done[*index] && !claimed[*index] && has_piece(*index) && self.rarity[*index] > 0
            })
            .min_by_key(|index| (self.rarity[*index], *index))
        else {
            return Ok(None);
        };
        claimed[candidate] = true;
        Ok(Some(candidate))
    }

    fn release(&self, index: usize) -> Result<()> {
        let mut claimed = self
            .claimed
            .lock()
            .map_err(|_| anyhow::anyhow!("piece claim state was poisoned"))?;
        *claimed
            .get_mut(index)
            .context("piece index is out of range")? = false;
        Ok(())
    }

    fn complete(&self, index: usize) -> Result<()> {
        let mut done = self
            .done
            .lock()
            .map_err(|_| anyhow::anyhow!("piece completion state was poisoned"))?;
        *done.get_mut(index).context("piece index is out of range")? = true;
        drop(done);
        self.release(index)
    }

    fn completed(&self) -> Result<usize> {
        Ok(self
            .done
            .lock()
            .map_err(|_| anyhow::anyhow!("piece completion state was poisoned"))?
            .iter()
            .filter(|done| **done)
            .count())
    }
}

struct Peer {
    stream: TcpStream,
    availability: Option<Vec<bool>>,
}

impl Peer {
    fn might_have(&self, index: usize) -> bool {
        self.availability
            .as_ref()
            .and_then(|bits| bits.get(index))
            .copied()
            .unwrap_or(true)
    }

    fn download_piece(&mut self, index: usize, torrent: &Torrent) -> Result<Vec<u8>> {
        let piece_offset = index
            .checked_mul(torrent.piece_length)
            .context("piece offset overflow")?;
        let total_length = usize::try_from(torrent.total_length)
            .context("torrent is too large for this platform")?;
        let piece_size = total_length
            .checked_sub(piece_offset)
            .context("piece offset is outside the torrent")?
            .min(torrent.piece_length);
        let mut blocks = Vec::new();
        let mut begin = 0;
        while begin < piece_size {
            let length = (piece_size - begin).min(BLOCK_SIZE);
            blocks.push((begin, length));
            begin += length;
        }
        let mut received = vec![None; blocks.len()];
        let mut in_flight = std::collections::HashMap::new();
        let mut next_block = 0;

        while received.iter().any(Option::is_none) {
            while next_block < blocks.len() && in_flight.len() < MAX_IN_FLIGHT_BLOCKS {
                let (begin, block_length) = blocks[next_block];
                let mut request = Vec::with_capacity(12);
                request.extend_from_slice(&(index as u32).to_be_bytes());
                request.extend_from_slice(&(begin as u32).to_be_bytes());
                request.extend_from_slice(&(block_length as u32).to_be_bytes());
                self.send_message(6, &request)
                    .context("could not request piece block")?;
                in_flight.insert(begin, block_length);
                next_block += 1;
            }

            match self.read_message()? {
                Message::KeepAlive => {}
                Message::Message(1, _) => {}
                Message::Message(0, _) => bail!("peer choked us"),
                Message::Message(4, payload) if payload.len() == 4 => {
                    let piece_index =
                        u32::from_be_bytes(payload.try_into().expect("4 bytes")) as usize;
                    if let Some(availability) = &mut self.availability
                        && let Some(bit) = availability.get_mut(piece_index)
                    {
                        *bit = true;
                    }
                }

                Message::Message(7, payload) if payload.len() >= 8 => {
                    let received_index =
                        u32::from_be_bytes(payload[0..4].try_into().expect("4 bytes")) as usize;
                    let begin =
                        u32::from_be_bytes(payload[4..8].try_into().expect("4 bytes")) as usize;
                    if received_index != index {
                        continue;
                    }
                    let Some(expected_length) = in_flight.remove(&begin) else {
                        continue;
                    };
                    let block = &payload[8..];
                    if block.len() != expected_length {
                        bail!("peer sent a block with an unexpected length");
                    }
                    let block_index = blocks
                        .iter()
                        .position(|(block_begin, _)| *block_begin == begin)
                        .context("peer returned an unrequested block offset")?;
                    received[block_index] = Some(block.to_vec());
                }
                Message::Message(_, _) => {}
            }
        }

        let mut piece = Vec::with_capacity(piece_size);
        for block in received {
            piece.extend(block.context("piece download ended with a missing block")?);
        }
        Ok(piece)
    }

    fn send_message(&mut self, id: u8, payload: &[u8]) -> Result<()> {
        let length = u32::try_from(payload.len() + 1).context("peer message is too large")?;
        self.stream.write_all(&length.to_be_bytes())?;
        self.stream.write_all(&[id])?;
        self.stream.write_all(payload)?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Message> {
        let mut length = [0; 4];
        self.stream
            .read_exact(&mut length)
            .context("peer disconnected while reading a message")?;
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 {
            return Ok(Message::KeepAlive);
        }
        if length > MAX_MESSAGE_SIZE {
            bail!("peer message exceeds the 1 MiB safety limit");
        }
        let mut message = vec![0; length];
        self.stream.read_exact(&mut message)?;
        Ok(Message::Message(message[0], message[1..].to_vec()))
    }
}

enum Message {
    KeepAlive,
    Message(u8, Vec<u8>),
}

fn connect(address: SocketAddr, torrent: &Torrent, peer_id: &[u8; 20]) -> Result<Peer> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(8))
        .with_context(|| format!("connection to {address} failed"))?;
    stream.set_read_timeout(Some(Duration::from_secs(90)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    let mut handshake = Vec::with_capacity(68);
    handshake.push(19);
    handshake.extend_from_slice(b"BitTorrent protocol");
    handshake.extend_from_slice(&[0; 8]);
    handshake.extend_from_slice(&torrent.info_hash);
    handshake.extend_from_slice(peer_id);
    stream
        .write_all(&handshake)
        .context("could not send handshake")?;

    let mut response = [0; 68];
    stream
        .read_exact(&mut response)
        .context("could not read peer handshake")?;
    if response[0] != 19 || &response[1..20] != b"BitTorrent protocol" {
        bail!("peer returned an invalid BitTorrent handshake");
    }
    if response[28..48] != torrent.info_hash {
        bail!("peer handshake has a different info hash");
    }

    let mut peer = Peer {
        stream,
        availability: None,
    };
    peer.send_message(2, &[])
        .context("could not send interested message")?;
    peer.wait_until_unchoked(torrent.piece_hashes.len())?;
    Ok(peer)
}

impl Peer {
    fn wait_until_unchoked(&mut self, piece_count: usize) -> Result<()> {
        loop {
            match self.read_message()? {
                Message::KeepAlive => {}
                Message::Message(1, _) => return Ok(()),
                Message::Message(5, payload) => {
                    let needed = piece_count.div_ceil(8);
                    if payload.len() != needed {
                        bail!("peer bitfield has an unexpected length");
                    }
                    let mut pieces = Vec::with_capacity(piece_count);
                    for index in 0..piece_count {
                        pieces.push(payload[index / 8] & (0x80 >> (index % 8)) != 0);
                    }
                    self.availability = Some(pieces);
                }
                Message::Message(4, payload) if payload.len() == 4 => {
                    let index = u32::from_be_bytes(payload.try_into().expect("4 bytes")) as usize;
                    let availability = self
                        .availability
                        .get_or_insert_with(|| vec![false; piece_count]);
                    if let Some(bit) = availability.get_mut(index) {
                        *bit = true;
                    }
                }
                Message::Message(_, _) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_least_available_piece_first() {
        let scheduler = PieceScheduler::new(vec![false; 4], vec![4, 1, 2, 1]);
        assert_eq!(scheduler.claim(|_| true).unwrap(), Some(1));
        assert_eq!(scheduler.claim(|_| true).unwrap(), Some(3));
        assert_eq!(scheduler.claim(|_| true).unwrap(), Some(2));
        assert_eq!(scheduler.claim(|_| true).unwrap(), Some(0));
        assert_eq!(scheduler.claim(|_| true).unwrap(), None);
    }

    #[test]
    fn only_claims_pieces_the_peer_advertises() {
        let scheduler = PieceScheduler::new(vec![false; 3], vec![1, 2, 1]);
        assert_eq!(scheduler.claim(|index| index == 1).unwrap(), Some(1));
        assert_eq!(scheduler.claim(|index| index == 1).unwrap(), None);
    }

    #[test]
    fn pipelines_and_reorders_piece_blocks() {
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let piece_data = (0..BLOCK_SIZE + 17)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let expected_data = piece_data.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut requests = Vec::new();
            for _ in 0..2 {
                let mut length = [0; 4];
                stream.read_exact(&mut length).unwrap();
                let length = u32::from_be_bytes(length) as usize;
                let mut message = vec![0; length];
                stream.read_exact(&mut message).unwrap();
                assert_eq!(message[0], 6);
                let payload = &message[1..];
                let index = u32::from_be_bytes(payload[0..4].try_into().unwrap());
                let begin = u32::from_be_bytes(payload[4..8].try_into().unwrap()) as usize;
                let block_length = u32::from_be_bytes(payload[8..12].try_into().unwrap()) as usize;
                assert_eq!(index, 0);
                requests.push((begin, block_length));
            }

            for (begin, length) in requests.into_iter().rev() {
                let mut payload = Vec::new();
                payload.extend_from_slice(&0_u32.to_be_bytes());
                payload.extend_from_slice(&(begin as u32).to_be_bytes());
                payload.extend_from_slice(&expected_data[begin..begin + length]);
                stream
                    .write_all(&((payload.len() + 1) as u32).to_be_bytes())
                    .unwrap();
                stream.write_all(&[7]).unwrap();
                stream.write_all(&payload).unwrap();
            }
        });

        let stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut peer = Peer {
            stream,
            availability: Some(vec![true]),
        };
        let torrent = Torrent {
            name: "piece".to_owned(),
            files: Vec::new(),
            piece_length: piece_data.len(),
            piece_hashes: vec![[0; 20]],
            info_hash: [0; 20],
            announce_urls: Vec::new(),
            total_length: piece_data.len() as u64,
        };

        assert_eq!(peer.download_piece(0, &torrent).unwrap(), piece_data);
        server.join().unwrap();
    }
}
