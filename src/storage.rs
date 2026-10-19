use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::torrent::Torrent;

pub struct Storage {
    files: Vec<File>,
    offsets: Vec<u64>,
    total_length: u64,
}

impl Storage {
    pub fn create(torrent: &Torrent, destination: &Path) -> Result<Self> {
        let root = if torrent.files.len() == 1 {
            destination.to_owned()
        } else {
            destination.join(&torrent.name)
        };
        fs::create_dir_all(&root)
            .with_context(|| format!("could not create directory {}", root.display()))?;
        let canonical_root = fs::canonicalize(&root)
            .with_context(|| format!("could not resolve output directory {}", root.display()))?;
        let mut files = Vec::with_capacity(torrent.files.len());
        let mut offsets = Vec::with_capacity(torrent.files.len());
        let mut offset = 0_u64;

        for torrent_file in &torrent.files {
            let path = safe_join(&root, &torrent_file.path)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("could not create directory {}", parent.display()))?;
                let canonical_parent = fs::canonicalize(parent)
                    .with_context(|| format!("could not resolve directory {}", parent.display()))?;
                if !canonical_parent.starts_with(&canonical_root) {
                    bail!("torrent output path escapes the destination directory");
                }
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("could not create output file {}", path.display()))?;
            file.set_len(torrent_file.length)
                .with_context(|| format!("could not size output file {}", path.display()))?;
            offsets.push(offset);
            offset = offset
                .checked_add(torrent_file.length)
                .context("torrent file offsets overflow")?;
            files.push(file);
        }

        Ok(Self {
            files,
            offsets,
            total_length: torrent.total_length,
        })
    }

    pub fn write_piece(&mut self, index: usize, piece_length: usize, bytes: &[u8]) -> Result<()> {
        let start = (index as u64)
            .checked_mul(piece_length as u64)
            .context("piece offset overflow")?;
        let end = start
            .checked_add(bytes.len() as u64)
            .context("piece end offset overflow")?;
        if end > self.total_length {
            bail!("piece extends beyond the torrent's total size");
        }

        for (file_index, file) in self.files.iter_mut().enumerate() {
            let file_start = self.offsets[file_index];
            let file_end = self
                .offsets
                .get(file_index + 1)
                .copied()
                .unwrap_or(self.total_length);
            let overlap_start = start.max(file_start);
            let overlap_end = end.min(file_end);
            if overlap_start >= overlap_end {
                continue;
            }
            let source_start = (overlap_start - start) as usize;
            let source_end = (overlap_end - start) as usize;
            file.seek(SeekFrom::Start(overlap_start - file_start))?;
            file.write_all(&bytes[source_start..source_end])
                .context("could not write downloaded piece")?;
        }
        Ok(())
    }

    pub fn read_range(&mut self, start: u64, length: usize) -> Result<Vec<u8>> {
        let end = start
            .checked_add(length as u64)
            .context("read range end offset overflow")?;
        if end > self.total_length {
            bail!("requested data extends beyond the torrent's total size");
        }

        let mut bytes = vec![0; length];
        for (file_index, file) in self.files.iter_mut().enumerate() {
            let file_start = self.offsets[file_index];
            let file_end = self
                .offsets
                .get(file_index + 1)
                .copied()
                .unwrap_or(self.total_length);
            let overlap_start = start.max(file_start);
            let overlap_end = end.min(file_end);
            if overlap_start >= overlap_end {
                continue;
            }
            let destination_start = (overlap_start - start) as usize;
            let destination_end = (overlap_end - start) as usize;
            file.seek(SeekFrom::Start(overlap_start - file_start))?;
            file.read_exact(&mut bytes[destination_start..destination_end])
                .context("could not read requested torrent data")?;
        }
        Ok(bytes)
    }
}

fn safe_join(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut path = root.to_owned();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(value) => path.push(value),
            _ => bail!("torrent contains an unsafe output path"),
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::torrent::TorrentFile;

    #[test]
    fn writes_piece_bytes_across_file_boundaries() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let destination = std::env::temp_dir().join(format!("manta-storage-test-{unique}"));
        let torrent = Torrent {
            name: "bundle".to_owned(),
            files: vec![
                TorrentFile {
                    path: PathBuf::from("nested").join("first.bin"),
                    length: 3,
                },
                TorrentFile {
                    path: PathBuf::from("second.bin"),
                    length: 3,
                },
            ],
            piece_length: 4,
            piece_hashes: Vec::new(),
            info_hash: [0; 20],
            announce_urls: Vec::new(),
            total_length: 6,
        };

        let mut storage = Storage::create(&torrent, &destination).unwrap();
        storage.write_piece(0, 4, &[1, 2, 3, 4]).unwrap();
        storage.write_piece(1, 4, &[5, 6]).unwrap();

        assert_eq!(
            fs::read(destination.join("bundle").join("nested").join("first.bin")).unwrap(),
            [1, 2, 3]
        );
        assert_eq!(
            fs::read(destination.join("bundle").join("second.bin")).unwrap(),
            [4, 5, 6]
        );
        assert_eq!(storage.read_range(1, 4).unwrap(), [2, 3, 4, 5]);
        drop(storage);

        let single_destination =
            std::env::temp_dir().join(format!("manta-single-storage-{unique}"));
        let single_torrent = Torrent {
            name: "single.bin".to_owned(),
            files: vec![TorrentFile {
                path: PathBuf::from("single.bin"),
                length: 3,
            }],
            piece_length: 3,
            piece_hashes: Vec::new(),
            info_hash: [0; 20],
            announce_urls: Vec::new(),
            total_length: 3,
        };
        let mut single_storage = Storage::create(&single_torrent, &single_destination).unwrap();
        single_storage.write_piece(0, 3, &[7, 8, 9]).unwrap();
        drop(single_storage);
        assert_eq!(
            fs::read(single_destination.join("single.bin")).unwrap(),
            [7, 8, 9]
        );

        fs::remove_dir_all(single_destination).unwrap();
        fs::remove_dir_all(destination).unwrap();
    }
}
