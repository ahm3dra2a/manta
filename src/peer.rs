use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};

use crate::torrent::Torrent;

const BLOCK_SIZE: usize = 16 * 1024;
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

pub fn download(
    torrent: &Torrent,
    peers: &[SocketAddr],
    peer_id: &[u8; 20],
    downloaded: &mut [bool],
    mut save_piece: impl FnMut(usize, &[u8]) -> Result<()>,
) -> Result<usize> {
    if downloaded.len() != torrent.piece_hashes.len() {
        bail!("download progress does not match the torrent's piece count");
    }
    let before = downloaded.iter().filter(|done| **done).count();
    for address in peers {
        if downloaded.iter().all(|done| *done) {
            break;
        }
        eprintln!("Connecting to {address}...");
        let mut peer = match connect(*address, torrent, peer_id) {
            Ok(peer) => peer,
            Err(error) => {
                eprintln!("  Could not connect: {error:#}");
                continue;
            }
        };
        for (index, expected_hash) in torrent.piece_hashes.iter().enumerate() {
            if downloaded[index] || !peer.might_have(index) {
                continue;
            }
            match peer.download_piece(index, torrent) {
                Ok(piece) if Sha1::digest(&piece).as_slice() == expected_hash => {
                    save_piece(index, &piece)?;
                    downloaded[index] = true;
                    let completed = downloaded.iter().filter(|done| **done).count();
                    eprintln!("Downloaded piece {}/{}", completed, downloaded.len());
                }
                Ok(_) => eprintln!("  Peer {address} sent corrupt piece {index}"),
                Err(error) => {
                    eprintln!("  Peer {address} failed on piece {index}: {error:#}");
                    break;
                }
            }
        }
    }
    Ok(downloaded.iter().filter(|done| **done).count() - before)
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
        let piece_size = (torrent.total_length as usize - piece_offset).min(torrent.piece_length);
        let mut piece = Vec::with_capacity(piece_size);
        while piece.len() < piece_size {
            let block_length = (piece_size - piece.len()).min(BLOCK_SIZE);
            let mut request = Vec::with_capacity(12);
            request.extend_from_slice(&(index as u32).to_be_bytes());
            request.extend_from_slice(&(piece.len() as u32).to_be_bytes());
            request.extend_from_slice(&(block_length as u32).to_be_bytes());
            self.send_message(6, &request)
                .context("could not request piece block")?;

            loop {
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
                        if received_index == index && begin == piece.len() {
                            let block = &payload[8..];
                            if block.len() != block_length {
                                bail!("peer sent a block with an unexpected length");
                            }
                            piece.extend_from_slice(block);
                            break;
                        }
                    }
                    Message::Message(_, _) => {}
                }
            }
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
