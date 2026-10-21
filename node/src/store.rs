//! Append-only block log persistence (`node::store`, §8.7).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use tracing::{info, warn};
use wire::{Block, MAX_FRAME_BYTES, codec};

/// Append-only writer for `blocks.log`.
pub struct BlockLogWriter {
    file: File,
}

impl BlockLogWriter {
    /// Appends a length-delimited (`u32` BE) encoded `Block`.
    pub fn append(&mut self, block: &Block) -> std::io::Result<()> {
        let bytes = codec::encode(block)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let len = u32::try_from(bytes.len())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "block too large"))?;
        let mut record = Vec::with_capacity(4 + bytes.len());
        record.extend_from_slice(&len.to_be_bytes());
        record.extend_from_slice(&bytes);
        self.file.write_all(&record)?;
        self.file.flush()
    }

    /// Flushes and fsyncs the underlying file.
    pub fn flush_and_sync(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        self.file.sync_all()
    }
}

/// Opens and replays `<data-dir>/blocks.log` (§8.7).
pub struct BlockStore;

impl BlockStore {
    /// Opens `blocks.log` (creating it if needed), reads every complete record, cuts off
    /// a truncated or corrupt tail, and returns the writer positioned at the end.
    pub fn open(path: &Path) -> std::io::Result<(BlockLogWriter, Vec<Block>)> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        let file_len = file.metadata()?.len();
        file.seek(SeekFrom::Start(0))?;

        let mut blocks = Vec::new();
        let mut valid_offset: u64 = 0;
        loop {
            let mut len_buf = [0u8; 4];
            match file.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let frame_len = u32::from_be_bytes(len_buf) as usize;
            if frame_len == 0 || frame_len > MAX_FRAME_BYTES {
                warn!(
                    offset = valid_offset,
                    frame_len, "Invalid record length in blocks.log; truncating"
                );
                break;
            }
            let mut buf = vec![0u8; frame_len];
            match file.read_exact(&mut buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    warn!(
                        offset = valid_offset,
                        "Truncated trailing record in blocks.log; cutting it off"
                    );
                    break;
                }
                Err(e) => return Err(e),
            }
            match codec::decode::<Block>(&buf) {
                Ok(block) => {
                    blocks.push(block);
                    valid_offset += 4 + frame_len as u64;
                }
                Err(err) => {
                    warn!(offset = valid_offset, error = %err, "Corrupt record in blocks.log; truncating");
                    break;
                }
            }
        }

        if valid_offset < file_len {
            file.set_len(valid_offset)?;
        }
        file.seek(SeekFrom::End(0))?;
        if !blocks.is_empty() {
            info!(
                count = blocks.len(),
                "Loaded persisted blocks from blocks.log"
            );
        }
        Ok((BlockLogWriter { file }, blocks))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::{BlockBody, BlockHeader, Digest, FwContext, PUBLIC_KEY_LEN, PublicKey, Target};

    fn block(height: u64) -> Block {
        let d = Digest([height as u8; 64]);
        Block {
            header: BlockHeader {
                context: FwContext {
                    chain_id: 1,
                    genesis_id: d,
                    protocol_version: 2,
                    crypto_suite_id: 1,
                    program_id: d,
                    air_id: d,
                    generator_version: 2,
                    parent_canonical_id: d,
                    stable_anchor_id: d,
                    height,
                    miner_policy_digest: d,
                    payout_policy_digest: d,
                    attempt_nonce: 0,
                    body_core_root: d,
                    parent_derived_difficulty_context: d,
                    time_bucket: 0,
                },
                output_root: d,
                work_id: d,
                block_id: d,
                share_ticket: d,
                seal_ticket: d,
                seal_target: Target(u128::MAX),
                share_target: Target(u128::MAX),
                miner_key: PublicKey::from_slice(&[height as u8; PUBLIC_KEY_LEN]).unwrap(),
                signature: chain::keys::unsigned(),
            },
            body: BlockBody {
                jobs: vec![],
                shares: vec![],
                leases: vec![],
                receipts: vec![],
                fee_transitions: vec![],
                ledger_digest: d,
            },
        }
    }

    #[test]
    fn log_round_trips_and_cuts_a_torn_tail() {
        let dir = std::env::temp_dir().join(format!("plau-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blocks.log");
        let _ = std::fs::remove_file(&path);

        let (mut writer, loaded) = BlockStore::open(&path).unwrap();
        assert!(loaded.is_empty());
        writer.append(&block(1)).unwrap();
        writer.append(&block(2)).unwrap();
        writer.flush_and_sync().unwrap();
        drop(writer);
        let good_len = std::fs::metadata(&path).unwrap().len();

        // Simulate a crash in the middle of a third record.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[0, 0, 4, 0, 1, 2, 3]).unwrap();
        drop(f);

        let (mut writer, loaded) = BlockStore::open(&path).unwrap();
        assert_eq!(loaded, vec![block(1), block(2)]);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
        writer.append(&block(3)).unwrap();
        drop(writer);
        let (_, loaded) = BlockStore::open(&path).unwrap();
        assert_eq!(loaded.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
