use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};

use crate::bencode::{self, Value};

#[derive(Debug, Clone)]
pub struct TorrentFile {
    pub path: PathBuf,
    pub length: u64,
}

#[derive(Debug)]
pub struct Torrent {
    pub name: String,
    pub files: Vec<TorrentFile>,
    pub piece_length: usize,
    pub piece_hashes: Vec<[u8; 20]>,
    pub info_hash: [u8; 20],
    pub announce_urls: Vec<String>,
    pub total_length: u64,
}

impl Torrent {
    pub fn from_bytes(input: &[u8]) -> Result<Self> {
        if input.len() > 16 * 1024 * 1024 {
            bail!("torrent file is larger than the 16 MiB safety limit");
        }
        let parsed = bencode::parse(input).context("invalid torrent bencode")?;
        let root = &parsed.value;
        let info = root
            .get(b"info")
            .context("torrent is missing its info dictionary")?;
        let info_span = find_info_span(input)?;
        let info_hash: [u8; 20] = Sha1::digest(&input[info_span.0..info_span.1]).into();

        let name = required_bytes(info, b"name", "info.name")?;
        let name = safe_component(name, "torrent name")?;
        let piece_length = required_integer(info, b"piece length", "info.piece length")?;
        let piece_length = usize::try_from(piece_length).context("piece length is out of range")?;
        if piece_length == 0 {
            bail!("piece length must be greater than zero");
        }
        let hashes = required_bytes(info, b"pieces", "info.pieces")?;
        if !hashes.len().is_multiple_of(20) {
            bail!("info.pieces length must be a multiple of 20 bytes");
        }
        let piece_hashes: Vec<[u8; 20]> = hashes
            .chunks(20)
            .map(|chunk| chunk.try_into().expect("20-byte chunk"))
            .collect();

        let files = if let Some(files) = info.get(b"files") {
            let mut result = Vec::new();
            for entry in files.as_list().context("info.files must be a list")? {
                let length = required_integer(entry, b"length", "file length")?;
                let length = u64::try_from(length).context("file length cannot be negative")?;
                let components = entry
                    .get(b"path")
                    .context("file entry is missing path")?
                    .as_list()
                    .context("file path must be a list")?;
                if components.is_empty() {
                    bail!("file path cannot be empty");
                }
                let mut path = PathBuf::new();
                for component in components {
                    let component = safe_component(
                        component
                            .as_bytes()
                            .context("file path component must be bytes")?,
                        "file path component",
                    )?;
                    path.push(component);
                }
                result.push(TorrentFile { path, length });
            }
            result
        } else {
            let length = required_integer(info, b"length", "info.length")?;
            let length = u64::try_from(length).context("file length cannot be negative")?;
            vec![TorrentFile {
                path: PathBuf::from(&name),
                length,
            }]
        };
        if files.is_empty() {
            bail!("torrent must contain at least one file");
        }
        let total_length = files.iter().try_fold(0_u64, |total, file| {
            total
                .checked_add(file.length)
                .context("total torrent size overflow")
        })?;
        let expected_piece_count = if total_length == 0 {
            0
        } else {
            total_length.div_ceil(piece_length as u64)
        };
        if expected_piece_count != piece_hashes.len() as u64 {
            bail!(
                "torrent has {} piece hashes, but its files require {expected_piece_count}",
                piece_hashes.len()
            );
        }

        let mut announce_urls = Vec::new();
        if let Some(url) = root.get(b"announce") {
            add_tracker(url, &mut announce_urls)?;
        }
        if let Some(tiers) = root.get(b"announce-list") {
            for tier in tiers.as_list().context("announce-list must be a list")? {
                for url in tier
                    .as_list()
                    .context("announce-list tier must be a list")?
                {
                    add_tracker(url, &mut announce_urls)?;
                }
            }
        }
        if announce_urls.is_empty() {
            bail!("torrent does not contain an HTTP tracker URL");
        }

        Ok(Self {
            name,
            files,
            piece_length,
            piece_hashes,
            info_hash,
            announce_urls,
            total_length,
        })
    }
}

fn find_info_span(input: &[u8]) -> Result<(usize, usize)> {
    let mut position = 0;
    if input.get(position) != Some(&b'd') {
        bail!("torrent root must be a dictionary");
    }
    position += 1;
    while input.get(position) != Some(&b'e') {
        let key = read_raw_bytes(input, &mut position)?;
        let value = bencode::parse_prefix(&input[position..])?;
        if key == b"info" {
            return Ok((position, position + value.end));
        }
        position += value.end;
    }
    bail!("torrent is missing its info dictionary")
}

fn read_raw_bytes(input: &[u8], position: &mut usize) -> Result<Vec<u8>> {
    let start = *position;
    while input.get(*position).is_some_and(u8::is_ascii_digit) {
        *position += 1;
    }
    if input.get(*position) != Some(&b':') || start == *position {
        bail!("invalid torrent dictionary key");
    }
    let length: usize = std::str::from_utf8(&input[start..*position])?
        .parse()
        .context("torrent dictionary key length is out of range")?;
    *position += 1;
    let end = position
        .checked_add(length)
        .context("torrent dictionary key length overflow")?;
    let bytes = input
        .get(*position..end)
        .context("torrent dictionary key is truncated")?
        .to_vec();
    *position = end;
    Ok(bytes)
}

fn required_bytes<'a>(value: &'a Value, key: &[u8], label: &str) -> Result<&'a [u8]> {
    value
        .get(key)
        .with_context(|| format!("torrent is missing {label}"))?
        .as_bytes()
        .with_context(|| format!("{label} must be bytes"))
}

fn required_integer(value: &Value, key: &[u8], label: &str) -> Result<i64> {
    value
        .get(key)
        .with_context(|| format!("torrent is missing {label}"))?
        .as_integer()
        .with_context(|| format!("{label} must be an integer"))
}

fn safe_component(bytes: &[u8], label: &str) -> Result<String> {
    let component = std::str::from_utf8(bytes).with_context(|| format!("{label} must be UTF-8"))?;
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains(['/', '\\', ':', '\0'])
        || component.ends_with(['.', ' '])
    {
        bail!("unsafe {label}: {component:?}");
    }
    Ok(component.to_owned())
}

fn add_tracker(value: &Value, urls: &mut Vec<String>) -> Result<()> {
    let bytes = value.as_bytes().context("tracker URL must be bytes")?;
    let url = std::str::from_utf8(bytes).context("tracker URL must be UTF-8")?;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Ok(());
    }
    if !urls.iter().any(|existing| existing == url) {
        urls.push(url.to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_the_original_info_bytes_and_reads_single_file_metadata() {
        let info = b"d6:lengthi3e4:name4:test12:piece lengthi3e6:pieces20:\
                     \x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00e";
        let mut torrent = b"d8:announce14:http://tracker4:info".to_vec();
        torrent.extend_from_slice(info);
        torrent.push(b'e');
        let parsed = Torrent::from_bytes(&torrent).unwrap();
        assert_eq!(parsed.name, "test");
        assert_eq!(parsed.total_length, 3);
        assert_eq!(parsed.piece_hashes.len(), 1);
        assert_eq!(parsed.info_hash, Sha1::digest(info).as_slice());
    }

    #[test]
    fn rejects_parent_directory_components() {
        let torrent = b"d8:announce14:http://tracker4:infod5:filesld6:lengthi1e4:pathl2:..ee4:name4:test12:piece lengthi1e6:pieces20:00000000000000000000ee";
        assert!(Torrent::from_bytes(torrent).is_err());
    }
}
