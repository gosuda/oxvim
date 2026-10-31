use std::io::Cursor;

use ox_editor::shada::{Stream, StreamEntry};
use ox_types::{ApiError, Object};

const MAX_CONTEXT_ENTRY_KBYTE: usize = 10 * 1024;

pub fn bytes_to_readfile_lines(data: &[u8]) -> Vec<Object> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut items = Vec::new();
    let mut start = 0;
    for (index, &byte) in data.iter().enumerate() {
        if byte == b'\n' {
            items.push(Object::String(nl_to_nul_item(&data[start..index])));
            start = index + 1;
        }
    }
    items.push(Object::String(nl_to_nul_item(&data[start..])));
    items
}

fn nl_to_nul_item(piece: &[u8]) -> ox_types::OxStr {
    let converted: Vec<u8> = piece
        .iter()
        .map(|&byte| if byte == 0 { b'\n' } else { byte })
        .collect();
    ox_types::OxStr(converted)
}

pub fn readfile_lines_to_bytes(lines: &[Object]) -> Result<Vec<u8>, ApiError> {
    if lines.is_empty() {
        return Ok(Vec::new());
    }
    let mut chunks = Vec::with_capacity(lines.len());
    for item in lines {
        let Object::String(s) = item else {
            return Err(ApiError::exception(
                "E474: Failed to convert list to msgpack string buffer",
            ));
        };
        let converted: Vec<u8> = s
            .as_bytes()
            .iter()
            .map(|&byte| if byte == b'\n' { 0 } else { byte })
            .collect();
        chunks.push(converted);
    }
    Ok(chunks.join(&b'\n'))
}

pub fn write_shada_entries(entries: &[StreamEntry]) -> Result<Vec<u8>, ApiError> {
    let stream = Stream {
        entries: entries.to_vec(),
    };
    let mut bytes = Vec::new();
    stream
        .write(&mut bytes, 0)
        .map_err(|error| ApiError::exception(error.to_string()))?;
    Ok(bytes)
}

pub fn read_shada_entries(bytes: &[u8]) -> Result<Vec<StreamEntry>, ApiError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let stream = Stream::read(Cursor::new(bytes), MAX_CONTEXT_ENTRY_KBYTE)
        .map_err(|error| ApiError::exception(error.to_string()))?;
    Ok(stream.entries)
}
