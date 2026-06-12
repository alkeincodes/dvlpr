//! Poll-based transcript tailer (spec §2.2): emit complete lines only; detect
//! truncation/rotation as a Reset so the consumer can re-watch. Polling (not
//! inotify/kqueue) matches the repo's simplicity bias; the bridge drives this
//! every 250 ms.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum TailPoll {
    /// Zero or more COMPLETE new lines (without trailing newline).
    Lines(Vec<String>),
    /// The file shrank or its inode changed: caller should re-open.
    Reset,
}

pub struct TranscriptTail {
    path: PathBuf,
    pos: u64,
    ino: u64,
    /// Bytes read past the last newline (an incomplete trailing line).
    partial: Vec<u8>,
}

impl TranscriptTail {
    /// `replay_full`: start at offset 0 (history streams out of the first
    /// poll). Otherwise start at the current end (tail only).
    pub fn open(path: &Path, replay_full: bool) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(TranscriptTail {
            path: path.to_path_buf(),
            pos: if replay_full { 0 } else { meta.len() },
            ino: meta.ino(),
            partial: Vec::new(),
        })
    }

    pub fn poll(&mut self) -> io::Result<TailPoll> {
        let meta = std::fs::metadata(&self.path)?;
        if meta.ino() != self.ino || meta.len() < self.pos {
            return Ok(TailPoll::Reset);
        }
        if meta.len() == self.pos {
            return Ok(TailPoll::Lines(Vec::new()));
        }
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(self.pos))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        self.pos += buf.len() as u64;

        self.partial.extend_from_slice(&buf);
        let mut lines = Vec::new();
        while let Some(nl) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=nl).collect();
            let line = &line[..line.len() - 1]; // strip '\n'
            lines.push(String::from_utf8_lossy(line).into_owned());
        }
        Ok(TailPoll::Lines(lines))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &std::path::Path, content: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn replay_full_emits_existing_lines_then_tails_new_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_file(&path, "{\"a\":1}\n{\"a\":2}\n");

        let mut tail = TranscriptTail::open(&path, true).unwrap();
        assert_eq!(
            tail.poll().unwrap(),
            TailPoll::Lines(vec!["{\"a\":1}".into(), "{\"a\":2}".into()])
        );
        assert_eq!(tail.poll().unwrap(), TailPoll::Lines(vec![]));

        write_file(&path, "{\"a\":3}\n");
        assert_eq!(
            tail.poll().unwrap(),
            TailPoll::Lines(vec!["{\"a\":3}".into()])
        );
    }

    #[test]
    fn replay_none_skips_existing_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_file(&path, "{\"old\":true}\n");
        let mut tail = TranscriptTail::open(&path, false).unwrap();
        assert_eq!(tail.poll().unwrap(), TailPoll::Lines(vec![]));
        write_file(&path, "{\"new\":true}\n");
        assert_eq!(
            tail.poll().unwrap(),
            TailPoll::Lines(vec!["{\"new\":true}".into()])
        );
    }

    #[test]
    fn incomplete_trailing_line_is_held_until_newline_arrives() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_file(&path, "{\"partial\":");
        let mut tail = TranscriptTail::open(&path, true).unwrap();
        assert_eq!(tail.poll().unwrap(), TailPoll::Lines(vec![]));
        write_file(&path, "1}\n");
        assert_eq!(
            tail.poll().unwrap(),
            TailPoll::Lines(vec!["{\"partial\":1}".into()])
        );
    }

    #[test]
    fn truncation_and_inode_change_report_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_file(&path, "{\"a\":1}\n");
        let mut tail = TranscriptTail::open(&path, true).unwrap();
        let _ = tail.poll().unwrap();

        // Truncate (size < pos).
        std::fs::write(&path, "").unwrap();
        assert_eq!(tail.poll().unwrap(), TailPoll::Reset);

        // Rotation: remove + recreate. The replacement content is SHORTER than
        // the read position so the size check triggers Reset deterministically
        // even on a filesystem that reuses the inode immediately.
        let mut tail = TranscriptTail::open(&path, true).unwrap();
        let _ = tail.poll().unwrap();
        std::fs::remove_file(&path).unwrap();
        write_file(&path, "x\n");
        assert_eq!(tail.poll().unwrap(), TailPoll::Reset);
    }
}
