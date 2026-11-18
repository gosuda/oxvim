//! `ShaDa` `MessagePack` stream reading, writing, and timestamp merge.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};

use rmpv::Value;
use thiserror::Error;

/// Known `ShaDa` entry types from `shada.c`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u64)]
pub enum EntryType {
    /// Debug/header metadata.
    Header = 1,
    /// Last search pattern.
    SearchPattern = 2,
    /// Last substitute string.
    SubString = 3,
    /// Command/search/expression/input/debug history.
    History = 4,
    /// Named register.
    Register = 5,
    /// Global variable.
    Variable = 6,
    /// Global file mark.
    GlobalMark = 7,
    /// Jump-list entry.
    Jump = 8,
    /// Buffer list.
    BufferList = 9,
    /// Buffer-local file mark.
    LocalMark = 10,
    /// Change-list entry.
    Change = 11,
}

impl EntryType {
    const fn from_raw(raw: u64) -> Option<Self> {
        Some(match raw {
            1 => Self::Header,
            2 => Self::SearchPattern,
            3 => Self::SubString,
            4 => Self::History,
            5 => Self::Register,
            6 => Self::Variable,
            7 => Self::GlobalMark,
            8 => Self::Jump,
            9 => Self::BufferList,
            10 => Self::LocalMark,
            11 => Self::Change,
            _ => return None,
        })
    }
}

/// One `ShaDa` stream entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Numeric entry type; unknown values are retained.
    pub type_id: u64,
    /// Unix timestamp in seconds.
    pub timestamp: u64,
    /// Type-specific `MessagePack` value.
    pub data: Value,
}

impl Entry {
    /// Constructs a known entry.
    #[must_use]
    pub const fn new(kind: EntryType, timestamp: u64, data: Value) -> Self {
        Self {
            type_id: kind as u64,
            timestamp,
            data,
        }
    }

    /// Returns the known entry type, if any.
    #[must_use]
    pub const fn kind(&self) -> Option<EntryType> {
        EntryType::from_raw(self.type_id)
    }
}

/// A sequence of `ShaDa` entries.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShaDa {
    /// Entries in stream order.
    pub entries: Vec<Entry>,
}

/// `ShaDa` stream error.
#[derive(Debug, Error)]
pub enum ShaDaError {
    /// Stream I/O failed.
    #[error("ShaDa I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// A `MessagePack` value was malformed.
    #[error("malformed ShaDa MessagePack: {0}")]
    Decode(String),
    /// An entry prefix was not an unsigned integer.
    #[error("malformed ShaDa entry prefix")]
    Prefix,
    /// A declared entry length exceeded the remaining bytes or address space.
    #[error("invalid ShaDa entry length")]
    Length,
}

impl ShaDa {
    /// Reads the concatenated type/timestamp/length/payload stream.
    ///
    /// # Errors
    ///
    /// Returns the reader's I/O error, [`ShaDaError::Prefix`] for a
    /// non-integer entry prefix, [`ShaDaError::Length`] for a declared
    /// length that exceeds the remaining bytes or address space, and
    /// [`ShaDaError::Decode`] for a malformed `MessagePack` payload.
    pub fn read(mut reader: impl Read, max_kbyte: usize) -> Result<Self, ShaDaError> {
        let mut entries = Vec::new();
        while let Some(type_id) = read_uint(&mut reader, true)? {
            let timestamp = read_uint(&mut reader, false)?.ok_or(ShaDaError::Length)?;
            let length_u64 = read_uint(&mut reader, false)?.ok_or(ShaDaError::Length)?;
            let max_length = u64::try_from(isize::MAX).map_err(|_| ShaDaError::Length)?;
            if length_u64 > max_length {
                return Err(ShaDaError::Length);
            }
            let length = usize::try_from(length_u64).map_err(|_| ShaDaError::Length)?;
            if max_kbyte != 0 && length > max_kbyte.saturating_mul(1024) {
                skip_bytes(&mut reader, length)?;
                continue;
            }

            let mut payload = Vec::with_capacity(length);
            reader
                .by_ref()
                .take(length_u64)
                .read_to_end(&mut payload)?;
            if payload.len() != length {
                return Err(ShaDaError::Length);
            }
            let mut payload_cursor = Cursor::new(payload.as_slice());
            let data = rmpv::decode::read_value(&mut payload_cursor)
                .map_err(|error| ShaDaError::Decode(error.to_string()))?;
            entries.push(Entry {
                type_id,
                timestamp,
                data,
            });
        }
        Ok(Self { entries })
    }

    /// Writes entries, omitting payloads above `max_kbyte` as Neovim does.
    ///
    /// # Errors
    ///
    /// Returns the writer's I/O error, [`ShaDaError::Decode`] if a payload
    /// cannot be re-encoded, and [`ShaDaError::Length`] if a payload
    /// length is not representable.
    pub fn write(&self, mut writer: impl Write, max_kbyte: usize) -> Result<(), ShaDaError> {
        for entry in &self.entries {
            let mut payload = Vec::new();
            rmpv::encode::write_value(&mut payload, &entry.data)
                .map_err(|error| ShaDaError::Decode(error.to_string()))?;
            if max_kbyte != 0 && payload.len() > max_kbyte.saturating_mul(1024) {
                continue;
            }
            write_uint(&mut writer, entry.type_id)?;
            write_uint(&mut writer, entry.timestamp)?;
            write_uint(
                &mut writer,
                u64::try_from(payload.len()).map_err(|_| ShaDaError::Length)?,
            )?;
            writer.write_all(&payload)?;
        }
        Ok(())
    }

    /// Merges two writers, selecting the newer timestamp for each identity.
    #[must_use]
    pub fn merge(&self, other: &Self) -> Self {
        let mut merged: BTreeMap<Vec<u8>, Entry> = BTreeMap::new();
        for entry in self.entries.iter().chain(&other.entries) {
            let key = merge_key(entry);
            match merged.get(&key) {
                Some(existing) if existing.timestamp > entry.timestamp => {}
                _ => {
                    merged.insert(key, entry.clone());
                }
            }
        }
        let mut entries: Vec<Entry> = merged.into_values().collect();
        entries.sort_by_key(|entry| (entry.type_id, entry.timestamp));
        Self { entries }
    }
}

fn merge_key(entry: &Entry) -> Vec<u8> {
    let mut key = entry.type_id.to_be_bytes().to_vec();
    match entry.kind() {
        Some(EntryType::Header | EntryType::SubString | EntryType::BufferList) => {}
        Some(EntryType::SearchPattern) => {
            if let Some(value) = map_value(&entry.data, b"ss") {
                append_value(&mut key, value);
            } else {
                append_value(&mut key, &Value::Boolean(false));
            }
        }
        Some(EntryType::Register | EntryType::GlobalMark) => {
            if let Some(value) = map_value(&entry.data, b"n") {
                append_value(&mut key, value);
            }
        }
        Some(EntryType::LocalMark) => {
            for field in [b"f".as_slice(), b"n".as_slice()] {
                if let Some(value) = map_value(&entry.data, field) {
                    append_value(&mut key, value);
                }
            }
        }
        Some(EntryType::Variable) => {
            if let Some(value) = array_value(&entry.data, 0) {
                append_value(&mut key, value);
            }
        }
        Some(EntryType::History) => {
            for index in 0..=1 {
                if let Some(value) = array_value(&entry.data, index) {
                    append_value(&mut key, value);
                }
            }
        }
        Some(EntryType::Jump | EntryType::Change) | None => append_value(&mut key, &entry.data),
    }
    key
}

fn append_value(bytes: &mut Vec<u8>, value: &Value) {
    let _ = rmpv::encode::write_value(bytes, value);
}

fn array_value(value: &Value, index: usize) -> Option<&Value> {
    value.as_array()?.get(index)
}

fn map_value<'a>(value: &'a Value, key: &[u8]) -> Option<&'a Value> {
    value.as_map()?.iter().find_map(|(candidate, value)| {
        let matches = candidate
            .as_str()
            .is_some_and(|text| text.as_bytes() == key)
            || candidate.as_slice() == Some(key);
        matches.then_some(value)
    })
}
fn read_uint(
    reader: &mut impl Read,
    allow_eof: bool,
) -> Result<Option<u64>, ShaDaError> {
    let mut prefix = [0; 1];
    if reader.read(&mut prefix)? == 0 {
        return if allow_eof {
            Ok(None)
        } else {
            Err(ShaDaError::Length)
        };
    }
    let width = match prefix[0] {
        0x00..=0x7f => return Ok(Some(u64::from(prefix[0]))),
        0xcc => 1,
        0xcd => 2,
        0xce => 4,
        0xcf => 8,
        _ => return Err(ShaDaError::Prefix),
    };
    let mut bytes = [0; 8];
    reader
        .read_exact(&mut bytes[8 - width..])
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::UnexpectedEof => ShaDaError::Length,
            _ => ShaDaError::Io(error),
        })?;
    Ok(Some(u64::from_be_bytes(bytes)))
}

fn skip_bytes(reader: &mut impl Read, mut remaining: usize) -> Result<(), ShaDaError> {
    let mut buffer = [0; 8192];
    while remaining != 0 {
        let chunk = remaining.min(buffer.len());
        let read = reader.read(&mut buffer[..chunk])?;
        if read == 0 {
            return Err(ShaDaError::Length);
        }
        remaining -= read;
    }
    Ok(())
}

fn write_uint(writer: &mut impl Write, value: u64) -> Result<(), ShaDaError> {
    rmpv::encode::write_value(writer, &Value::from(value))
        .map_err(|error| ShaDaError::Decode(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use super::*;

    struct CountingReader {
        inner: Cursor<Vec<u8>>,
        largest_request: usize,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.largest_request = self.largest_request.max(buffer.len());
            self.inner.read(buffer)
        }
    }

    #[test]
    fn read_skips_oversized_payload_without_buffering_the_file() {
        let stream = ShaDa {
            entries: vec![
                Entry::new(
                    EntryType::SubString,
                    1,
                    Value::Binary(vec![b'x'; 128 * 1024]),
                ),
                Entry::new(EntryType::Header, 2, Value::Map(vec![])),
            ],
        };
        let mut bytes = Vec::new();
        stream.write(&mut bytes, 0).expect("test stream must encode");
        let expected = stream.entries[1].clone();
        let mut reader = CountingReader {
            inner: Cursor::new(bytes),
            largest_request: 0,
        };

        let decoded = ShaDa::read(&mut reader, 1).expect("stream must decode");

        assert_eq!(decoded.entries, vec![expected]);
        assert!(reader.largest_request <= 8192);
    }

    #[test]
    fn write_then_read_round_trips_entries() {
        let stream = ShaDa {
            entries: vec![
                Entry::new(EntryType::Header, 1, Value::Map(vec![])),
                Entry::new(
                    EntryType::SubString,
                    2,
                    Value::Array(vec![Value::Binary(b"replacement".to_vec())]),
                ),
            ],
        };
        let mut bytes = Vec::new();
        stream.write(&mut bytes, 0).expect("test stream must encode");

        assert_eq!(
            ShaDa::read(Cursor::new(bytes), 0).expect("stream must decode"),
            stream
        );
    }
}
