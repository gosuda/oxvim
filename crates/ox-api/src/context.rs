//! Editor context save/load API.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::session::ApiSession;
use crate::{ApiError, Dict, Object, OxStr, Registry, RegistryError, api};

#[path = "shada_codec.rs"]
mod shada_codec;

#[cfg(test)]
pub(crate) use shada_codec::bytes_to_readfile_lines;

const ALL_TYPES: [&str; 5] = ["regs", "jumps", "bufs", "gvars", "funcs"];
const VALID_REQUEST_TYPES: [&str; 6] = ["regs", "jumps", "bufs", "gvars", "funcs", "sfuncs"];

fn object_type_name(value: &Object) -> &'static str {
    match value {
        Object::Nil => "nil",
        Object::Boolean(_) => "Boolean",
        Object::Integer(_) => "Integer",
        Object::Float(_) => "Float",
        Object::String(_) => "String",
        Object::Array(_) => "Array",
        Object::Dict(_) => "Dict",
        Object::LuaRef(_) => "Function",
        Object::Buffer(_) => "Buffer",
        Object::Window(_) => "Window",
        Object::Tabpage(_) => "Tabpage",
    }
}

fn selected_types(opts: &Dict) -> Result<Vec<String>, ApiError> {
    for (key, _) in opts.iter() {
        if key.as_bytes() != b"types" {
            return Err(ApiError::validation(format!(
                "Invalid key: '{}'",
                key.to_string_lossy()
            )));
        }
    }
    let Some(value) = opts.get(&OxStr::from("types")) else {
        return Ok(ALL_TYPES.iter().map(ToString::to_string).collect());
    };
    let Object::Array(values) = value else {
        return Err(ApiError::validation(format!(
            "Invalid 'types': expected Array, got {}",
            object_type_name(value)
        )));
    };
    if values.is_empty() {
        return Ok(ALL_TYPES.iter().map(ToString::to_string).collect());
    }
    let mut requested = Vec::with_capacity(values.len());
    for item in values {
        let Object::String(s) = item else {
            return Err(ApiError::validation(format!(
                "Invalid 'type': expected String, got {}",
                object_type_name(item)
            )));
        };
        let name = s.to_string_lossy().into_owned();
        if !VALID_REQUEST_TYPES.contains(&name.as_str()) {
            return Err(ApiError::validation(format!("Invalid 'type': '{name}'")));
        }
        requested.push(name);
    }
    Ok(requested)
}

pub(crate) fn display_cells(line: &[u8]) -> usize {
    use unicode_width::UnicodeWidthChar;

    let mut cells = 0;
    let mut index = 0;
    while index < line.len() {
        let first = line[index];
        if first < 0x80 {
            cells += match first {
                0x00..=0x08 | 0x0a..=0x1f | 0x7f => 2,
                _ => 1,
            };
            index += 1;
            continue;
        }
        let width = match first {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 1,
        };
        if width > 1
            && index + width <= line.len()
            && let Ok(text) = std::str::from_utf8(&line[index..index + width])
        {
            for ch in text.chars() {
                cells += ch.width().unwrap_or(1);
            }
            index += width;
            continue;
        }
        cells += 4;
        index += 1;
    }
    cells
}

#[api(since = 6)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded context options"
)]
pub fn nvim_get_context(session: &ApiSession, opts: Dict) -> Result<Dict, ApiError> {
    let requested = selected_types(&opts)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut values = Vec::with_capacity(ALL_TYPES.len());
    for &kind in &ALL_TYPES {
        let is_requested = requested.iter().any(|item| item == kind);
        let lines = if is_requested {
            match kind {
                "regs" => {
                    let entries = session.with_editor(|ed| {
                        ox_editor::shada::register_entries(ed, -1, timestamp)
                    });
                    let bytes = shada_codec::write_shada_entries(&entries)?;
                    shada_codec::bytes_to_readfile_lines(&bytes)
                }
                "jumps" => {
                    let entries =
                        session.with_editor(|ed| ox_editor::shada::jump_entries(ed, timestamp));
                    let bytes = shada_codec::write_shada_entries(&entries)?;
                    shada_codec::bytes_to_readfile_lines(&bytes)
                }
                "bufs" => {
                    let entry = session
                        .with_editor(|ed| ox_editor::shada::buffer_list_entry(ed, timestamp));
                    let bytes = shada_codec::write_shada_entries(&[entry])?;
                    shada_codec::bytes_to_readfile_lines(&bytes)
                }
                "gvars" => {
                    let entries =
                        session.with_editor(|ed| ox_editor::shada::gvar_entries(ed, timestamp));
                    let bytes = shada_codec::write_shada_entries(&entries)?;
                    shada_codec::bytes_to_readfile_lines(&bytes)
                }
                "funcs" => Vec::new(),
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };
        values.push((OxStr::from(kind), Object::Array(lines)));
    }
    let context = Dict(values);
    session.with_state_mut(|state| state.saved_context = Some(context.clone()));
    Ok(context)
}

#[api(since = 6)]
pub fn nvim_load_context(session: &ApiSession, dict: Dict) -> Result<Object, ApiError> {
    let mut regs_bytes: Option<Vec<u8>> = None;
    let mut jumps_bytes: Option<Vec<u8>> = None;
    let mut bufs_bytes: Option<Vec<u8>> = None;
    let mut gvars_bytes: Option<Vec<u8>> = None;
    let mut funcs_cmds: Vec<OxStr> = Vec::new();

    for (key, value) in &dict.0 {
        let Object::Array(lines) = value else {
            continue;
        };
        match key.as_bytes() {
            b"regs" => {
                regs_bytes = Some(shada_codec::readfile_lines_to_bytes(lines)?);
            }
            b"jumps" => {
                jumps_bytes = Some(shada_codec::readfile_lines_to_bytes(lines)?);
            }
            b"bufs" => {
                bufs_bytes = Some(shada_codec::readfile_lines_to_bytes(lines)?);
            }
            b"gvars" => {
                gvars_bytes = Some(shada_codec::readfile_lines_to_bytes(lines)?);
            }
            b"funcs" => {
                for item in lines {
                    if let Object::String(body) = item {
                        funcs_cmds.push(body.clone());
                    }
                }
            }
            _ => {}
        }
    }

    session.with_editor_mut(|editor| {
        let registers = editor.registers_mut();
        for name in std::iter::once('-').chain('0'..='9').chain('a'..='z') {
            let _ = registers.clear_for_setreg(name);
        }
    });

    for (bytes, type_id) in [
        (regs_bytes, 5_u64),
        (jumps_bytes, 8_u64),
        (bufs_bytes, 9_u64),
        (gvars_bytes, 6_u64),
    ] {
        let Some(bytes) = bytes else {
            continue;
        };
        let Ok(entries) = shada_codec::read_shada_entries(&bytes) else {
            continue;
        };
        session.with_editor_mut(|editor| {
            for entry in entries {
                if entry.type_id != type_id {
                    continue;
                }
                match type_id {
                    5 => ox_editor::shada::apply_register_entry(editor, &entry.data),
                    8 => ox_editor::shada::apply_jump_entry(editor, &entry.data),
                    9 => ox_editor::shada::apply_buffer_list_entry(editor, &entry.data),
                    6 => ox_editor::shada::apply_gvar_entry(editor, &entry.data),
                    _ => {}
                }
            }
        });
    }

    for cmd in funcs_cmds {
        let _ = crate::global::nvim_command(session, cmd);
    }

    session.with_state_mut(|state| state.saved_context = Some(dict));
    Ok(Object::Nil)
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_get_context__API_META(), nvim_get_context__API_DISPATCH)?;
    registry.register(
        nvim_load_context__API_META(),
        nvim_load_context__API_DISPATCH,
    )?;
    Ok(())
}
