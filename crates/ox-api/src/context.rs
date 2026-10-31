//! Editor context save/load API.

use ox_editor::{MarkTarget, RegisterContent, RegisterKind};
use unicode_width::UnicodeWidthChar;

use crate::session::ApiSession;
use crate::{ApiError, Dict, Object, OxStr, Registry, RegistryError, api};

const TYPES: [&str; 5] = ["regs", "jumps", "bufs", "gvars", "funcs"];

fn selected(opts: &Dict) -> Result<Vec<String>, ApiError> {
    let Some(value) = opts.get(&OxStr::from("types")) else {
        return Ok(TYPES.iter().map(ToString::to_string).collect());
    };
    let Object::Array(values) = value else {
        return Err(ApiError::validation("types must be an Array"));
    };
    values
        .iter()
        .map(|value| {
            let Object::String(value) = value else {
                return Err(ApiError::validation("context type must be a String"));
            };
            let name = String::from_utf8(value.0.clone())
                .map_err(|_| ApiError::validation("context type must be UTF-8"))?;
            if !TYPES.contains(&name.as_str()) {
                return Err(ApiError::validation(format!(
                    "Invalid context type: {name}"
                )));
            }
            Ok(name)
        })
        .collect()
}

fn register_type(kind: RegisterKind) -> OxStr {
    match kind {
        RegisterKind::CharacterWise => OxStr::from("v"),
        RegisterKind::LineWise => OxStr::from("V"),
        RegisterKind::BlockWise { width } => {
            let mut value = vec![0x16];
            value.extend(width.to_string().as_bytes());
            OxStr(value)
        }
    }
}

/// Upstream `MAXCOL` (`pos_defs.h:19`), the column and block-width upper bound.
const MAXCOL: usize = 0x7fff_ffff;

enum RegisterShape {
    Character,
    Line,
    Block { width: Option<usize> },
}

fn parse_register_type(value: &[u8]) -> Result<RegisterShape, ApiError> {
    let Some(&first) = value.first() else {
        return Err(ApiError::validation("invalid context register type"));
    };
    match first {
        b'v' | b'c' => {
            if value.len() != 1 {
                return Err(ApiError::validation("invalid context register type"));
            }
            Ok(RegisterShape::Character)
        }
        b'V' | b'l' => {
            if value.len() != 1 {
                return Err(ApiError::validation("invalid context register type"));
            }
            Ok(RegisterShape::Line)
        }
        b'b' | 0x16 => {
            if value.len() == 1 {
                return Ok(RegisterShape::Block { width: None });
            }
            if !value[1..].iter().all(u8::is_ascii_digit) {
                return Err(ApiError::validation("invalid context register type"));
            }
            let width = value[1..].iter().try_fold(0_usize, |width, digit| {
                width
                    .checked_mul(10)?
                    .checked_add(usize::from(*digit - b'0'))
            });
            let Some(width) = width else {
                return Err(ApiError::validation("invalid context register type"));
            };
            if width == 0 || width > MAXCOL {
                return Err(ApiError::validation("invalid context register type"));
            }
            Ok(RegisterShape::Block { width: Some(width) })
        }
        _ => Err(ApiError::validation("invalid context register type")),
    }
}

/// Measures display cells in a row mirroring upstream `mb_string2cells_len`
/// (`register.c:update_yankreg_width`). Printable ASCII and tabs are one cell,
/// control bytes two, invalid bytes four, and Unicode characters their display width.
pub(crate) fn display_cells(line: &[u8]) -> usize {
    let mut cells = 0_usize;
    let mut index = 0;
    while index < line.len() {
        let first = line[index];
        if first < 0x80 {
            cells = cells.saturating_add(match first {
                0x00..=0x08 | 0x0a..=0x1f | 0x7f => 2,
                _ => 1,
            });
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
            let char_cells = text.chars().next().map_or(1, |character| {
                UnicodeWidthChar::width(character).unwrap_or(1).max(1)
            });
            cells = cells.saturating_add(char_cells);
            index += width;
            continue;
        }
        cells = cells.saturating_add(4);
        index += 1;
    }
    cells
}

fn save_registers(session: &ApiSession) -> Vec<Object> {
    session.with_editor(|editor| {
        ('0'..='9')
            .chain('a'..='z')
            .chain(['"', '-'])
            .filter_map(|name| {
                editor.registers().get(name).ok().flatten().map(|content| {
                    Object::Dict(Dict(vec![
                        (
                            OxStr::from("name"),
                            Object::String(OxStr::from(name.to_string().as_str())),
                        ),
                        (
                            OxStr::from("type"),
                            Object::String(register_type(content.kind())),
                        ),
                        (
                            OxStr::from("lines"),
                            Object::Array(
                                content
                                    .lines()
                                    .iter()
                                    .map(|line| Object::String(OxStr::from(line.as_slice())))
                                    .collect(),
                            ),
                        ),
                    ]))
                })
            })
            .collect()
    })
}

fn save_jumps(session: &ApiSession) -> Vec<Object> {
    session.with_editor(|editor| {
        editor
            .jumplist()
            .entries()
            .iter()
            .map(|jump| {
                let mut values = vec![
                    (
                        OxStr::from("lnum"),
                        Object::Integer(i64::try_from(jump.position.lnum).unwrap_or(i64::MAX)),
                    ),
                    (
                        OxStr::from("col"),
                        Object::Integer(i64::try_from(jump.position.col).unwrap_or(i64::MAX)),
                    ),
                ];
                match &jump.target {
                    MarkTarget::Buffer(buffer) => {
                        values.push((OxStr::from("bufnr"), Object::Integer(i64::from(*buffer))));
                    }
                    MarkTarget::File(path) => values.push((
                        OxStr::from("filename"),
                        Object::String(OxStr::from(path.to_string_lossy().as_bytes())),
                    )),
                }
                Object::Dict(Dict(values))
            })
            .collect()
    })
}

#[api(since = 6)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded context options"
)]
pub fn nvim_get_context(session: &ApiSession, opts: Dict) -> Result<Dict, ApiError> {
    let selected = selected(&opts)?;
    let mut values = Vec::new();
    for kind in selected {
        let value =
            match kind.as_str() {
                "regs" => Object::Array(save_registers(session)),
                "jumps" => Object::Array(save_jumps(session)),
                "bufs" => Object::Array(session.with_editor(|editor| {
                    editor.buffers().into_iter().map(Object::Buffer).collect()
                })),
                "gvars" => Object::Dict(session.with_editor(|editor| editor.gvars().clone())),
                "funcs" => Object::Array(Vec::new()),
                _ => continue,
            };
        values.push((OxStr::from(kind.as_str()), value));
    }
    let context = Dict(values);
    session.with_state_mut(|state| state.saved_context = Some(context.clone()));
    Ok(context)
}

fn load_registers(session: &ApiSession, values: &[Object]) -> Result<(), ApiError> {
    for value in values {
        let Object::Dict(entry) = value else {
            return Err(ApiError::validation(
                "context register must be a Dictionary",
            ));
        };
        let Some(Object::String(name)) = entry.get(&OxStr::from("name")) else {
            return Err(ApiError::validation("context register requires name"));
        };
        let name = std::str::from_utf8(name.as_bytes())
            .ok()
            .and_then(|name| name.chars().next())
            .ok_or_else(|| ApiError::validation("invalid context register name"))?;
        let shape = match entry.get(&OxStr::from("type")) {
            Some(Object::String(value)) => parse_register_type(value.as_bytes())?,
            Some(_) => return Err(ApiError::validation("invalid context register type")),
            None => RegisterShape::Character,
        };
        let Some(Object::Array(lines)) = entry.get(&OxStr::from("lines")) else {
            return Err(ApiError::validation("context register requires lines"));
        };
        let lines = lines
            .iter()
            .map(|line| match line {
                Object::String(line) => Ok(line.0.clone()),
                _ => Err(ApiError::validation("register line must be a String")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let kind = match shape {
            RegisterShape::Block { width } => {
                let max_cells = lines
                    .iter()
                    .map(|line| display_cells(line))
                    .max()
                    .unwrap_or(0);
                let width = match width {
                    Some(explicit) => explicit.max(max_cells),
                    None => max_cells.max(1),
                };
                if width > MAXCOL {
                    return Err(ApiError::validation("invalid context register type"));
                }
                RegisterKind::BlockWise { width }
            }
            RegisterShape::Line => RegisterKind::LineWise,
            RegisterShape::Character => RegisterKind::CharacterWise,
        };
        let content = RegisterContent::from_binary_lines(kind, lines);
        session.with_editor_mut(|editor| {
            editor
                .registers_mut()
                .set(name, content)
                .map_err(|error| ApiError::validation(error.to_string()))
        })?;
    }
    Ok(())
}

#[api(since = 6)]
pub fn nvim_load_context(session: &ApiSession, dict: Dict) -> Result<Object, ApiError> {
    for (key, value) in &dict.0 {
        match (key.as_bytes(), value) {
            (b"regs", Object::Array(values)) => load_registers(session, values)?,
            (b"gvars", Object::Dict(values)) => {
                session.with_editor_mut(|editor| *editor.gvars_mut() = values.clone());
            }
            (b"jumps" | b"bufs" | b"funcs", Object::Array(_)) => {}
            (b"regs" | b"gvars" | b"jumps" | b"bufs" | b"funcs", _) => {
                return Err(ApiError::validation(format!(
                    "invalid context value for {}",
                    key.to_string_lossy()
                )));
            }
            _ => {
                return Err(ApiError::validation(format!(
                    "Invalid context type: {}",
                    key.to_string_lossy()
                )));
            }
        }
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
