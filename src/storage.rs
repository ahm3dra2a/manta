use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};

use crate::torrent::Torrent;

const VERIFY_CHUNK_SIZE: usize = 64 * 1024;

pub struct Storage {
    files: Vec<File>,
    offsets: Vec<u64>,
    existing_files: Vec<bool>,
    total_length: u64,
}

impl Storage {
    pub fn create(torrent: &Torrent, destination: &Path) -> Result<Self> {
        Self::open_files(torrent, destination, false)
    }

    pub fn open_or_create(torrent: &Torrent, destination: &Path) -> Result<Self> {
        Self::open_files(torrent, destination, true)
    }

    fn open_files(torrent: &Torrent, destination: &Path, resume: bool) -> Result<Self> {
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
        let mut existing_files = Vec::with_capacity(torrent.files.len());
        let mut canonical_files = Vec::with_capacity(torrent.files.len());
        let mut offset = 0_u64;

        for torrent_file in &torrent.files {
            let path = safe_join(&root, &torrent_file.path)?;
            ensure_parent_directories(&root, &torrent_file.path, &canonical_root)?;
            let (file, existed) = if resume {
                match fs::symlink_metadata(&path) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() || !metadata.is_file() {
                            bail!(
                                "existing output path is not a regular file: {}",
                                path.display()
                            );
                        }
                        let file = OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(&path)
                            .with_context(|| {
                                format!("could not open existing output file {}", path.display())
                            })?;
                        let actual_length = file.metadata()?.len();
                        if actual_length != torrent_file.length {
                            bail!(
                                "existing output file {} has size {actual_length}, expected {}; refusing to modify it",
                                path.display(),
                                torrent_file.length
                            );
                        }
                        (file, true)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        (create_output_file(&path, torrent_file.length)?, false)
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("could not inspect output file {}", path.display())
                        });
                    }
                }
            } else {
                (create_output_file(&path, torrent_file.length)?, false)
            };
            let canonical_file = fs::canonicalize(&path)
                .with_context(|| format!("could not resolve output file {}", path.display()))?;
            if !canonical_file.starts_with(&canonical_root) {
                bail!("torrent output path escapes the destination directory");
            }
            if canonical_files.contains(&canonical_file) {
                bail!(
                    "torrent contains duplicate output paths: {}",
                    path.display()
                );
            }
            canonical_files.push(canonical_file);
            offsets.push(offset);
            existing_files.push(existed);
            offset = offset
                .checked_add(torrent_file.length)
                .context("torrent file offsets overflow")?;
            files.push(file);
        }
        if offset != torrent.total_length {
            bail!("torrent file sizes do not match the torrent's total size");
        }

        Ok(Self {
            files,
            offsets,
            existing_files,
            total_length: torrent.total_length,
        })
    }

    pub fn verify_pieces(&mut self, torrent: &Torrent) -> Result<Vec<bool>> {
        if self.total_length != torrent.total_length {
            bail!("storage size does not match the torrent's total size");
        }
        if torrent.piece_length == 0 {
            bail!("torrent piece length must be greater than zero");
        }
        let expected_piece_count = if torrent.total_length == 0 {
            0
        } else {
            torrent.total_length.div_ceil(torrent.piece_length as u64)
        };
        if expected_piece_count != torrent.piece_hashes.len() as u64 {
            bail!("torrent piece hashes do not match the torrent's total size");
        }

        let mut verified = Vec::with_capacity(torrent.piece_hashes.len());
        for (index, expected_hash) in torrent.piece_hashes.iter().enumerate() {
            let start = u64::try_from(index)
                .context("piece index is too large")?
                .checked_mul(
                    u64::try_from(torrent.piece_length).context("piece length is too large")?,
                )
                .context("piece offset overflow")?;
            let length = torrent.piece_size(index)?;
            if !self.range_is_existing(start, length)? {
                verified.push(false);
                continue;
            }
            let mut hasher = Sha1::new();
            let mut hashed = 0;
            while hashed < length {
                let chunk_length = (length - hashed).min(VERIFY_CHUNK_SIZE);
                let chunk_start = start
                    .checked_add(u64::try_from(hashed).context("piece offset is too large")?)
                    .context("piece offset overflow")?;
                hasher.update(self.read_range(chunk_start, chunk_length)?);
                hashed += chunk_length;
            }
            verified.push(hasher.finalize().as_slice() == expected_hash);
        }
        Ok(verified)
    }

    fn range_is_existing(&self, start: u64, length: usize) -> Result<bool> {
        let end = start
            .checked_add(u64::try_from(length).context("piece length is too large")?)
            .context("piece end offset overflow")?;
        if end > self.total_length {
            bail!("piece extends beyond the torrent's total size");
        }
        for (index, existed) in self.existing_files.iter().enumerate() {
            let file_start = self.offsets[index];
            let file_end = self
                .offsets
                .get(index + 1)
                .copied()
                .unwrap_or(self.total_length);
            if start.max(file_start) < end.min(file_end) && !existed {
                return Ok(false);
            }
        }
        Ok(true)
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

fn create_output_file(path: &Path, length: u64) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("could not create output file {}", path.display()))?;
    file.set_len(length)
        .with_context(|| format!("could not size output file {}", path.display()))?;
    Ok(file)
}

fn ensure_parent_directories(root: &Path, relative: &Path, canonical_root: &Path) -> Result<()> {
    let mut current = root.to_owned();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            match component {
                std::path::Component::Normal(value) => current.push(value),
                _ => bail!("torrent contains an unsafe output path"),
            }
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("torrent output path contains a symbolic link")
                }
                Ok(metadata) if !metadata.is_dir() => {
                    bail!(
                        "torrent output parent is not a directory: {}",
                        current.display()
                    )
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current).with_context(|| {
                        format!("could not create directory {}", current.display())
                    })?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("could not inspect directory {}", current.display())
                    });
                }
            }
            let canonical_parent = fs::canonicalize(&current)
                .with_context(|| format!("could not resolve directory {}", current.display()))?;
            if !canonical_parent.starts_with(canonical_root) {
                bail!("torrent output path escapes the destination directory");
            }
        }
    }
    Ok(())
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

    use sha1::{Digest, Sha1};

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

    #[test]
    fn resumes_verified_pieces_and_rejects_wrong_sized_files() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let destination = std::env::temp_dir().join(format!("manta-resume-test-{unique}"));
        let torrent = Torrent {
            name: "resume-bundle".to_owned(),
            files: vec![
                TorrentFile {
                    path: PathBuf::from("nested").join("first.bin"),
                    length: 2,
                },
                TorrentFile {
                    path: PathBuf::from("second.bin"),
                    length: 3,
                },
            ],
            piece_length: 3,
            piece_hashes: vec![Sha1::digest([1, 2, 3]).into(), Sha1::digest([4, 5]).into()],
            info_hash: [0; 20],
            announce_urls: Vec::new(),
            total_length: 5,
        };

        let mut storage = Storage::open_or_create(&torrent, &destination).unwrap();
        assert_eq!(storage.existing_files, [false, false]);
        assert_eq!(storage.verify_pieces(&torrent).unwrap(), [false, false]);
        storage.write_piece(0, 3, &[1, 2, 3]).unwrap();
        storage.write_piece(1, 3, &[9, 9]).unwrap();
        drop(storage);

        let mut storage = Storage::open_or_create(&torrent, &destination).unwrap();
        assert_eq!(storage.existing_files, [true, true]);
        assert_eq!(storage.verify_pieces(&torrent).unwrap(), [true, false]);
        storage.write_piece(1, 3, &[4, 5]).unwrap();
        drop(storage);

        let mut storage = Storage::open_or_create(&torrent, &destination).unwrap();
        assert_eq!(storage.verify_pieces(&torrent).unwrap(), [true, true]);
        drop(storage);

        let second_file = destination.join("resume-bundle").join("second.bin");
        fs::write(&second_file, [1, 2]).unwrap();
        let error = Storage::open_or_create(&torrent, &destination)
            .err()
            .expect("wrong-sized existing output file must be rejected");
        assert!(error.to_string().contains("refusing to modify it"));
        assert_eq!(fs::read(second_file).unwrap(), [1, 2]);
        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn rejects_duplicate_resume_output_paths() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let destination = std::env::temp_dir().join(format!("manta-duplicate-path-{unique}"));
        let torrent = Torrent {
            name: "duplicate-bundle".to_owned(),
            files: vec![
                TorrentFile {
                    path: PathBuf::from("same.bin"),
                    length: 1,
                },
                TorrentFile {
                    path: PathBuf::from("same.bin"),
                    length: 1,
                },
            ],
            piece_length: 1,
            piece_hashes: vec![[0; 20], [0; 20]],
            info_hash: [0; 20],
            announce_urls: Vec::new(),
            total_length: 2,
        };

        assert!(
            Storage::open_or_create(&torrent, &destination)
                .err()
                .expect("duplicate output paths must be rejected")
                .to_string()
                .contains("duplicate output paths")
        );
        assert_eq!(
            fs::metadata(destination.join("duplicate-bundle").join("same.bin"))
                .unwrap()
                .len(),
            1
        );
        fs::remove_dir_all(destination).unwrap();
    }
}
