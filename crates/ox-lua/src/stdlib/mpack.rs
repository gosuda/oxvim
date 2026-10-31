use std::collections::{HashMap, HashSet};
use std::io::{Cursor, ErrorKind};

use mlua::{
    AnyUserData, Function, Lua, LuaString, MetaMethod, MultiValue, Table, UserData,
    UserDataMethods, Value,
};
use rmpv::Value as MpackValue;

use crate::converter::{has_empty_dict_metatable, is_vim_nil};

const RECURSION_LIMIT: usize = 100;

#[derive(Clone, Debug)]
struct MpackExt {
    kind: i8,
    data: Vec<u8>,
}

impl UserData for MpackExt {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!(
                "vim.mpack.ext({}, {} bytes)",
                this.kind,
                this.data.len()
            ))
        });
    }
}

#[derive(Debug, Default)]
struct Packer {
    ext: Option<Table>,
    is_bin: Option<IsBin>,
}

#[derive(Clone, Debug)]
enum IsBin {
    Boolean(bool),
    Function(Function),
}

impl UserData for Packer {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Call, |lua, this, value: Value| {
            let value = lua_to_mpack(lua, &value, 0, &mut HashSet::new(), Some(this))?;
            let mut output = Vec::new();
            rmpv::encode::write_value(&mut output, &value).map_err(mlua::Error::external)?;
            lua.create_string(output)
        });
    }
}

#[derive(Debug)]
struct Unpacker {
    pending: Vec<u8>,
    ext: Table,
}

impl Unpacker {
    fn decode_next(
        &mut self,
        input: &[u8],
        start: usize,
    ) -> mlua::Result<(Option<MpackValue>, usize)> {
        decode_value(&mut self.pending, input, start)
    }
}

/// Decodes the next msgpack value, carrying partially received input across
/// calls in `pending`.
fn decode_value(
    pending: &mut Vec<u8>,
    input: &[u8],
    start: usize,
) -> mlua::Result<(Option<MpackValue>, usize)> {
    if start == 0 || start > input.len() {
        return Err(mlua::Error::runtime(
            "start position must be between 1 and the input string length",
        ));
    }
    let chunk = &input[start - 1..];
    let previous_len = pending.len();
    let mut scratch = Vec::new();
    let bytes = concat_chunks(pending, chunk, &mut scratch);
    let mut cursor = Cursor::new(bytes);
    match rmpv::decode::read_value(&mut cursor) {
        Ok(value) => {
            let consumed = usize::try_from(cursor.position())
                .map_err(mlua::Error::external)?
                .saturating_sub(previous_len);
            pending.clear();
            Ok((Some(value), start + consumed))
        }
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
            pending.clear();
            pending.extend_from_slice(bytes);
            Ok((None, input.len() + 1))
        }
        Err(error) => {
            pending.clear();
            Err(mlua::Error::runtime(format!("invalid msgpack: {error}")))
        }
    }
}

/// Returns `pending` followed by `chunk`, using `scratch` when both are
/// present.
fn concat_chunks<'a>(pending: &[u8], chunk: &'a [u8], scratch: &'a mut Vec<u8>) -> &'a [u8] {
    if pending.is_empty() {
        return chunk;
    }
    scratch.reserve(pending.len() + chunk.len());
    scratch.extend_from_slice(pending);
    scratch.extend_from_slice(chunk);
    scratch.as_slice()
}

impl UserData for Unpacker {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method_mut(
            MetaMethod::Call,
            |lua, this, (input, start): (LuaString, Option<usize>)| {
                let input = input.as_bytes();
                let (value, next) = this.decode_next(&input, start.unwrap_or(1))?;
                Ok((
                    match value {
                        Some(value) => mpack_to_lua_with_ext(lua, value, 0, Some(&this.ext))?,
                        None => Value::Nil,
                    },
                    next,
                ))
            },
        );
    }
}

#[derive(Debug)]
struct Session {
    unpacker: Option<AnyUserData>,
    buffer: Vec<u8>,
    pending: HashMap<u32, Value>,
    next_id: u32,
}

impl UserData for Session {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("request", |lua, this, data: Option<Value>| {
            let first = this.next_id;
            while this.pending.contains_key(&this.next_id) {
                this.next_id = next_request_id(this.next_id);
                if this.next_id == first {
                    return Err(mlua::Error::runtime("no free msgpack-rpc request ids"));
                }
            }
            let id = this.next_id;
            this.next_id = next_request_id(id);
            this.pending.insert(id, data.unwrap_or(Value::Nil));
            lua.create_string(rpc_prefix(4, 0, Some(id))?)
        });
        methods.add_method("reply", |lua, _, id: u32| {
            lua.create_string(rpc_prefix(4, 1, Some(id))?)
        });
        methods.add_method("notify", |lua, _, ()| {
            lua.create_string(rpc_prefix(3, 2, None)?)
        });
        methods.add_method_mut(
            "receive",
            |lua, this, (input, start): (LuaString, Option<usize>)| {
                session_receive(lua, this, &input.as_bytes(), start.unwrap_or(1))
            },
        );
    }
}

pub(super) fn install(lua: &Lua, vim: &Table) -> mlua::Result<()> {
    let module = lua.create_table()?;
    module.set(
        "Packer",
        lua.create_function(|lua, options: Option<Table>| {
            let ext = option_table(lua, options.as_ref(), "ext")?;
            let is_bin = match options
                .as_ref()
                .map(|options| options.get::<Value>("is_bin"))
                .transpose()?
            {
                None | Some(Value::Nil) => None,
                Some(Value::Boolean(value)) => Some(IsBin::Boolean(value)),
                Some(Value::Function(value)) => Some(IsBin::Function(value)),
                Some(_) => {
                    return Err(mlua::Error::runtime(
                        "\"is_bin\" option must be a boolean or function",
                    ));
                }
            };
            lua.create_userdata(Packer { ext, is_bin })
        })?,
    )?;
    module.set(
        "Unpacker",
        lua.create_function(|lua, options: Option<Table>| {
            let ext = option_table(lua, options.as_ref(), "ext")?.unwrap_or(lua.create_table()?);
            lua.create_userdata(Unpacker {
                pending: Vec::new(),
                ext,
            })
        })?,
    )?;
    module.set(
        "encode",
        lua.create_function(|lua, value: Value| {
            let mut active = HashSet::new();
            let value = lua_to_mpack(lua, &value, 0, &mut active, None)?;
            let mut output = Vec::new();
            rmpv::encode::write_value(&mut output, &value).map_err(mlua::Error::external)?;
            lua.create_string(output)
        })?,
    )?;
    module.set(
        "decode",
        lua.create_function(|lua, input: LuaString| {
            let bytes = input.as_bytes();
            let mut cursor = Cursor::new(bytes.as_ref());
            let value = rmpv::decode::read_value(&mut cursor)
                .map_err(|error| mlua::Error::runtime(format!("invalid msgpack: {error}")))?;
            if cursor.position() != bytes.len() as u64 {
                return Err(mlua::Error::runtime("invalid msgpack: trailing data"));
            }
            mpack_to_lua(lua, value, 0)
        })?,
    )?;
    let session_native = lua.create_function(|lua, options: Option<Value>| {
        // Upstream lmpack only treats a table first argument as options;
        // nil and non-table arguments create a session without unpacker.
        let unpacker = match options {
            Some(Value::Table(options)) => {
                let unpacker = match options.get::<Value>("unpack") {
                    Ok(Value::UserData(unpacker)) => unpacker,
                    _ => {
                        return Ok((
                            false,
                            Value::String(lua.create_string(
                                "\"unpack\" option must be a mpack.Unpacker instance",
                            )?),
                        ));
                    }
                };
                if unpacker.borrow::<Unpacker>().is_err() {
                    return Ok((
                        false,
                        Value::String(lua.create_string(
                            "\"unpack\" option must be a mpack.Unpacker instance",
                        )?),
                    ));
                }
                Some(unpacker)
            }
            _ => None,
        };
        let session = lua.create_userdata(Session {
            unpacker,
            buffer: Vec::new(),
            pending: HashMap::new(),
            next_id: 0,
        })?;
        Ok((true, Value::UserData(session)))
    })?;
    let session: Function = crate::vim::error_shim(lua)?.call(session_native)?;
    module.set("Session", session)?;
    module.set("NIL", vim.get::<Value>("NIL")?)?;
    vim.set("mpack", module.clone())?;
    let package: Table = lua.globals().get("package")?;
    let loaded: Table = package.get("loaded")?;
    loaded.set("mpack", module)
}

fn rpc_prefix(len: u32, kind: u32, id: Option<u32>) -> mlua::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(16);
    output.push(0x90 | u8::try_from(len).map_err(mlua::Error::external)?);
    rmpv::encode::write_value(&mut output, &MpackValue::from(kind))
        .map_err(mlua::Error::external)?;
    if let Some(id) = id {
        rmpv::encode::write_value(&mut output, &MpackValue::from(id))
            .map_err(mlua::Error::external)?;
    }
    Ok(output)
}

fn next_request_id(id: u32) -> u32 {
    if id == u32::MAX - 1 { 0 } else { id + 1 }
}

enum Step<T> {
    Ready(T),
    Incomplete,
    Invalid,
}

enum RpcFrame {
    Request(u32),
    Response(u32),
    Notification,
}

/// Implements `Session:receive` following lmpack.c: without an unpacker only
/// the msgpack-rpc header is consumed and three values are returned; with one
/// the unpacker also consumes method and arguments, returning five values.
fn session_receive(
    lua: &Lua,
    session: &mut Session,
    input: &[u8],
    start: usize,
) -> mlua::Result<MultiValue> {
    let Session {
        unpacker,
        buffer,
        pending,
        ..
    } = session;
    let Some(unpacker) = unpacker.as_ref() else {
        return receive_frame(lua, buffer, pending, input, start);
    };
    if start == 0 || start > input.len() {
        return Err(mlua::Error::runtime(
            "start position must be between 1 and the input string length",
        ));
    }
    let mut unpacker = unpacker.borrow_mut::<Unpacker>()?;
    let step = {
        let chunk = &input[start - 1..];
        let mut scratch = Vec::new();
        let bytes = concat_chunks(&unpacker.pending, chunk, &mut scratch);
        parse_rpc_frame(bytes)
    };
    match step {
        Step::Invalid => {
            unpacker.pending.clear();
            return Err(mlua::Error::runtime("invalid msgpack-rpc string"));
        }
        Step::Incomplete | Step::Ready(_) => {}
    }
    let (value, next) = unpacker.decode_next(input, start)?;
    let Some(MpackValue::Array(mut fields)) = value else {
        if value.is_none() {
            return Ok(MultiValue::from_iter([
                Value::Nil,
                Value::Nil,
                Value::Nil,
                Value::Nil,
                position_value(next)?,
            ]));
        }
        return Err(mlua::Error::runtime("invalid msgpack-rpc string"));
    };
    if fields.is_empty() {
        return Err(mlua::Error::runtime("invalid msgpack-rpc string"));
    }
    let kind = rpc_u32(&fields[0])?;
    let expected = if kind == 2 { 3 } else { 4 };
    if fields.len() != expected {
        return Err(mlua::Error::runtime("invalid msgpack-rpc string"));
    }
    let ext = Some(&unpacker.ext);
    let position = position_value(next)?;
    match kind {
        0 => {
            let id = rpc_u32(&fields[1])?;
            let method = mpack_to_lua_with_ext(lua, fields.remove(2), 0, ext)?;
            let args = mpack_to_lua_with_ext(lua, fields.remove(2), 0, ext)?;
            Ok(MultiValue::from_iter([
                Value::String(lua.create_string("request")?),
                Value::Integer(i64::from(id)),
                method,
                args,
                position,
            ]))
        }
        1 => {
            let id = rpc_u32(&fields[1])?;
            let callback = remove_pending_callback(pending, id)?;
            let error = mpack_to_lua_with_ext(lua, fields.remove(2), 0, ext)?;
            let result = mpack_to_lua_with_ext(lua, fields.remove(2), 0, ext)?;
            Ok(MultiValue::from_iter([
                Value::String(lua.create_string("response")?),
                callback,
                error,
                result,
                position,
            ]))
        }
        2 => {
            let method = mpack_to_lua_with_ext(lua, fields.remove(1), 0, ext)?;
            let args = mpack_to_lua_with_ext(lua, fields.remove(1), 0, ext)?;
            Ok(MultiValue::from_iter([
                Value::String(lua.create_string("notification")?),
                Value::Nil,
                method,
                args,
                position,
            ]))
        }
        _ => Err(mlua::Error::runtime("invalid msgpack-rpc string")),
    }
}

fn remove_pending_callback(pending: &mut HashMap<u32, Value>, id: u32) -> mlua::Result<Value> {
    pending
        .remove(&id)
        .ok_or_else(|| mlua::Error::runtime("invalid msgpack-rpc string"))
}

/// Header-only receive used by sessions without an unpacker, mirroring
/// `mpack_rpc_receive`: the payload stays in the caller's string.
fn receive_frame(
    lua: &Lua,
    buffer: &mut Vec<u8>,
    pending: &mut HashMap<u32, Value>,
    input: &[u8],
    start: usize,
) -> mlua::Result<MultiValue> {
    let (frame, next) = decode_rpc_header(buffer, input, start)?;
    let position = position_value(next)?;
    let Some(frame) = frame else {
        return Ok(MultiValue::from_iter([Value::Nil, Value::Nil, position]));
    };
    let mut values = MultiValue::new();
    match frame {
        RpcFrame::Request(id) => {
            values.push_back(Value::String(lua.create_string("request")?));
            values.push_back(Value::Integer(i64::from(id)));
        }
        RpcFrame::Response(id) => {
            let callback = remove_pending_callback(pending, id)?;
            values.push_back(Value::String(lua.create_string("response")?));
            values.push_back(callback);
        }
        RpcFrame::Notification => {
            values.push_back(Value::String(lua.create_string("notification")?));
            values.push_back(Value::Nil);
        }
    }
    values.push_back(position);
    Ok(values)
}

/// Parses the msgpack-rpc frame header, buffering incomplete input in
/// `buffer` across calls exactly like `mpack_rpc_session_t.reader`.
fn decode_rpc_header(
    buffer: &mut Vec<u8>,
    input: &[u8],
    start: usize,
) -> mlua::Result<(Option<RpcFrame>, usize)> {
    if start == 0 || start > input.len() {
        return Err(mlua::Error::runtime(
            "start position must be between 1 and the input string length",
        ));
    }
    let chunk = &input[start - 1..];
    let previous_len = buffer.len();
    let mut scratch = Vec::new();
    let bytes = concat_chunks(buffer, chunk, &mut scratch);
    match parse_rpc_frame(bytes) {
        Step::Ready((frame, header_len)) => {
            let consumed = header_len.saturating_sub(previous_len);
            buffer.clear();
            Ok((Some(frame), start + consumed))
        }
        Step::Incomplete => {
            buffer.clear();
            buffer.extend_from_slice(bytes);
            Ok((None, input.len() + 1))
        }
        Step::Invalid => {
            buffer.clear();
            Err(mlua::Error::runtime("invalid msgpack-rpc string"))
        }
    }
}

/// Mirrors `mpack_rpc_receive_tok`: array of length 3 or 4, a type uint of at
/// most one byte holding at most 2, and for request/response a request id
/// uint of at most four bytes.
fn parse_rpc_frame(bytes: &[u8]) -> Step<(RpcFrame, usize)> {
    let (mut offset, length) = match read_array_header(bytes) {
        Step::Ready(header) => header,
        Step::Incomplete => return Step::Incomplete,
        Step::Invalid => return Step::Invalid,
    };
    if length != 3 && length != 4 {
        return Step::Invalid;
    }
    let (used, kind) = match read_uint(&bytes[offset..], 1) {
        Step::Ready(uint) => uint,
        Step::Incomplete => return Step::Incomplete,
        Step::Invalid => return Step::Invalid,
    };
    offset += used;
    if kind > 2 {
        return Step::Invalid;
    }
    if kind == 2 {
        return if length == 3 {
            Step::Ready((RpcFrame::Notification, offset))
        } else {
            Step::Invalid
        };
    }
    if length != 4 {
        return Step::Invalid;
    }
    let (used, id) = match read_uint(&bytes[offset..], 4) {
        Step::Ready(uint) => uint,
        Step::Incomplete => return Step::Incomplete,
        Step::Invalid => return Step::Invalid,
    };
    offset += used;
    let Ok(id) = u32::try_from(id) else {
        return Step::Invalid;
    };
    Step::Ready((
        if kind == 0 {
            RpcFrame::Request(id)
        } else {
            RpcFrame::Response(id)
        },
        offset,
    ))
}

fn read_array_header(bytes: &[u8]) -> Step<(usize, u32)> {
    let Some(&first) = bytes.first() else {
        return Step::Incomplete;
    };
    match first {
        0x90..=0x9f => Step::Ready((1, u32::from(first & 0x0f))),
        0xdc => {
            let Some(rest) = bytes.get(1..3) else {
                return Step::Incomplete;
            };
            Step::Ready((3, u32::from(u16::from_be_bytes([rest[0], rest[1]]))))
        }
        0xdd => {
            let Some(rest) = bytes.get(1..5) else {
                return Step::Incomplete;
            };
            Step::Ready((
                5,
                u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]),
            ))
        }
        _ => Step::Invalid,
    }
}

/// Reads a msgpack uint token, returning the total consumed byte count and
/// value; `max_length` is the largest accepted token payload width in bytes.
fn read_uint(bytes: &[u8], max_length: usize) -> Step<(usize, u64)> {
    let Some(&first) = bytes.first() else {
        return Step::Incomplete;
    };
    let (consumed, length, value) = match first {
        0x00..=0x7f => (1usize, 1usize, u64::from(first)),
        0xcc => {
            let Some(rest) = bytes.get(1) else {
                return Step::Incomplete;
            };
            (2, 1, u64::from(*rest))
        }
        0xcd => {
            let Some(rest) = bytes.get(1..3) else {
                return Step::Incomplete;
            };
            (3, 2, u64::from(u16::from_be_bytes([rest[0], rest[1]])))
        }
        0xce => {
            let Some(rest) = bytes.get(1..5) else {
                return Step::Incomplete;
            };
            (
                5,
                4,
                u64::from(u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]])),
            )
        }
        0xcf => {
            let Some(rest) = bytes.get(1..9) else {
                return Step::Incomplete;
            };
            (
                9,
                8,
                u64::from_be_bytes([
                    rest[0], rest[1], rest[2], rest[3], rest[4], rest[5], rest[6], rest[7],
                ]),
            )
        }
        _ => return Step::Invalid,
    };
    if length > max_length {
        Step::Invalid
    } else {
        Step::Ready((consumed, value))
    }
}

fn position_value(next: usize) -> mlua::Result<Value> {
    let position = i64::try_from(next)
        .map_err(|_| mlua::Error::runtime("msgpack position exceeds platform limits"))?;
    Ok(Value::Integer(position))
}

fn rpc_u32(value: &MpackValue) -> mlua::Result<u32> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| mlua::Error::runtime("invalid msgpack-rpc string"))
}

/// Encodes a Lua double following upstream `.references/neovim/src/mpack/conv.c:108-145`:
/// - Infinities encode as 32-bit float because `(float)v == v` holds.
/// - -0.0 and 0.0 encode as integer 0.
/// - Finite numbers within `[-2^63, 0)` that equal their `i64` truncation encode as signed integer tokens.
/// - Finite numbers within `[0, 2^64)` that equal their `u64` truncation encode as unsigned integer tokens.
/// - Other finite doubles use F32 when `(float)v == v` else F64. NaN fails the single-precision check and stays F64.
fn pack_number(value: f64) -> MpackValue {
    if value.is_infinite() {
        return MpackValue::F32(if value.is_sign_negative() {
            f32::NEG_INFINITY
        } else {
            f32::INFINITY
        });
    }

    const INT64_MIN_DOUBLE: f64 = -9_223_372_036_854_775_808.0;
    const UINT64_MAX_DOUBLE: f64 = 18_446_744_073_709_551_616.0;

    if value == 0.0 {
        return MpackValue::Integer(0_u64.into());
    }

    if value < 0.0 && value >= INT64_MIN_DOUBLE {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "range guard guarantees value truncates to an in-range i64"
        )]
        let int_val = value as i64;
        #[expect(
            clippy::cast_precision_loss,
            reason = "compares whether the double is an exact integer"
        )]
        if (int_val as f64) == value {
            return MpackValue::Integer(int_val.into());
        }
    } else if value > 0.0 && value < UINT64_MAX_DOUBLE {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "range guard guarantees value truncates to an in-range u64"
        )]
        let uint_val = value as u64;
        #[expect(
            clippy::cast_precision_loss,
            reason = "compares whether the double is an exact integer"
        )]
        if (uint_val as f64) == value {
            return MpackValue::Integer(uint_val.into());
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "checking whether double fits in single-precision float"
    )]
    let single = value as f32;
    if f64::from(single) == value {
        MpackValue::F32(single)
    } else {
        MpackValue::F64(value)
    }
}

fn lua_to_mpack(
    lua: &Lua,
    value: &Value,
    depth: usize,
    active: &mut HashSet<usize>,
    packer: Option<&Packer>,
) -> mlua::Result<MpackValue> {
    if depth > RECURSION_LIMIT {
        return Err(mlua::Error::runtime("msgpack object is too deeply nested"));
    }
    if let (Some(packer), Value::Table(table)) = (packer, value)
        && let (Some(ext), Some(metatable)) = (&packer.ext, table.metatable())
        && let Value::Function(handler) = ext.raw_get::<Value>(metatable)?
    {
        let (kind, payload): (i64, LuaString) = handler.call(value.clone())?;
        let kind = i8::try_from(kind)
            .ok()
            .filter(|kind| *kind >= 0)
            .ok_or_else(|| mlua::Error::runtime("extension type must be between 0 and 127"))?;
        return Ok(MpackValue::Ext(kind, payload.as_bytes().to_vec()));
    }
    Ok(match value {
        Value::Nil => MpackValue::Nil,
        Value::Boolean(value) => MpackValue::Boolean(*value),
        Value::Integer(value) => MpackValue::Integer((*value).into()),
        Value::Number(value) => pack_number(*value),
        Value::String(value) => {
            let bytes = value.as_bytes();
            let is_bin = match packer.and_then(|packer| packer.is_bin.as_ref()) {
                Some(IsBin::Boolean(value)) => *value,
                Some(IsBin::Function(handler)) => handler.call::<bool>(value.clone())?,
                None => false,
            };
            if is_bin {
                MpackValue::Binary(bytes.to_vec())
            } else {
                match std::str::from_utf8(&bytes) {
                    Ok(text) => MpackValue::String(text.to_owned().into()),
                    Err(_) => MpackValue::Binary(bytes.to_vec()),
                }
            }
        }
        Value::Table(table) => table_to_mpack(lua, table, depth, active, packer)?,
        Value::UserData(_) if is_vim_nil(lua, value).map_err(mlua::Error::external)? => {
            MpackValue::Nil
        }
        Value::UserData(userdata) => {
            let extension = userdata.borrow::<MpackExt>().map_err(|_| {
                mlua::Error::runtime(
                    "cannot encode userdata other than vim.NIL or msgpack extension",
                )
            })?;
            MpackValue::Ext(extension.kind, extension.data.clone())
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "cannot encode {} as msgpack",
                other.type_name()
            )));
        }
    })
}

fn table_to_mpack(
    lua: &Lua,
    table: &Table,
    depth: usize,
    active: &mut HashSet<usize>,
    packer: Option<&Packer>,
) -> mlua::Result<MpackValue> {
    let pointer = table.to_pointer() as usize;
    if !active.insert(pointer) {
        return Err(mlua::Error::runtime("cannot encode recursive table"));
    }
    let mut entries = Vec::new();
    let mut max_index = 0usize;
    let mut array_only = !has_empty_dict_metatable(lua, table).map_err(mlua::Error::external)?;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        if let Value::Integer(index) = key
            && let Ok(array_index) = usize::try_from(index)
            && array_index > 0
        {
            max_index = max_index.max(array_index);
            entries.push((key, value));
            continue;
        }
        array_only = false;
        entries.push((key, value));
    }

    let result = if array_only && max_index == entries.len() {
        let mut values = vec![MpackValue::Nil; max_index];
        for (key, value) in &entries {
            let Value::Integer(index) = key else {
                continue;
            };
            let index = usize::try_from(*index)
                .map_err(|_| mlua::Error::runtime("msgpack array index exceeds platform limits"))?;
            values[index - 1] = lua_to_mpack(lua, value, depth + 1, active, packer)?;
        }
        MpackValue::Array(values)
    } else {
        let mut values = Vec::with_capacity(entries.len());
        for (key, value) in &entries {
            values.push((
                lua_to_mpack(lua, key, depth + 1, active, packer)?,
                lua_to_mpack(lua, value, depth + 1, active, packer)?,
            ));
        }
        MpackValue::Map(values)
    };
    active.remove(&pointer);
    Ok(result)
}

fn mpack_to_lua(lua: &Lua, value: MpackValue, depth: usize) -> mlua::Result<Value> {
    mpack_to_lua_with_ext(lua, value, depth, None)
}

fn mpack_to_lua_with_ext(
    lua: &Lua,
    value: MpackValue,
    depth: usize,
    ext: Option<&Table>,
) -> mlua::Result<Value> {
    if depth > RECURSION_LIMIT {
        return Err(mlua::Error::runtime("msgpack object is too deeply nested"));
    }
    match value {
        MpackValue::Nil => vim_nil(lua),
        MpackValue::Boolean(value) => Ok(Value::Boolean(value)),
        MpackValue::Integer(value) => {
            if let Some(value) = value.as_i64() {
                Ok(Value::Integer(value))
            } else if let Some(value) = value.as_u64() {
                if let Ok(value) = i64::try_from(value) {
                    Ok(Value::Integer(value))
                } else {
                    // msgpack u64 above i64::MAX cannot be a LuaJIT integer; it
                    // degrades to a double like Neovim's msgpack decode.
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "LuaJIT represents msgpack u64 values above i64::MAX as doubles"
                    )]
                    let number = value as f64;
                    Ok(Value::Number(number))
                }
            } else {
                Err(mlua::Error::runtime("invalid msgpack integer"))
            }
        }
        MpackValue::F32(value) => Ok(Value::Number(f64::from(value))),
        MpackValue::F64(value) => Ok(Value::Number(value)),
        MpackValue::String(value) => Ok(Value::String(lua.create_string(value.as_bytes())?)),
        MpackValue::Binary(value) => Ok(Value::String(lua.create_string(value)?)),
        MpackValue::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            for (index, value) in values.into_iter().enumerate() {
                table.raw_set(
                    index + 1,
                    mpack_to_lua_with_ext(lua, value, depth + 1, ext)?,
                )?;
            }
            Ok(Value::Table(table))
        }
        MpackValue::Map(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            if values.is_empty() {
                set_empty_dict_metatable(lua, &table)?;
            }
            for (key, value) in values {
                let key = mpack_to_lua_with_ext(lua, key, depth + 1, ext)?;
                if matches!(key, Value::Nil)
                    || is_vim_nil(lua, &key).map_err(mlua::Error::external)?
                {
                    return Err(mlua::Error::runtime("msgpack map contains nil key"));
                }
                table.raw_set(key, mpack_to_lua_with_ext(lua, value, depth + 1, ext)?)?;
            }
            Ok(Value::Table(table))
        }
        MpackValue::Ext(kind, data) => {
            if let Some(handler) = ext
                .and_then(|handlers| handlers.raw_get::<Value>(i64::from(kind)).ok())
                .and_then(|value| match value {
                    Value::Function(handler) => Some(handler),
                    _ => None,
                })
            {
                handler.call::<Value>((i64::from(kind), lua.create_string(data)?))
            } else if ext.is_some() {
                Ok(Value::String(lua.create_string(data)?))
            } else {
                Ok(Value::UserData(
                    lua.create_userdata(MpackExt { kind, data })?,
                ))
            }
        }
    }
}

fn option_table(lua: &Lua, options: Option<&Table>, name: &str) -> mlua::Result<Option<Table>> {
    let Some(options) = options else {
        return Ok(None);
    };
    match options.get::<Value>(name)? {
        Value::Nil => Ok(None),
        Value::Table(source) => {
            let copy = lua.create_table()?;
            for pair in source.pairs::<Value, Value>() {
                let (key, value) = pair?;
                copy.raw_set(key, value)?;
            }
            Ok(Some(copy))
        }
        _ => Err(mlua::Error::runtime(format!(
            "\"{name}\" option must be a table"
        ))),
    }
}

fn vim_nil(lua: &Lua) -> mlua::Result<Value> {
    let vim: Table = lua.globals().get("vim")?;
    vim.get("NIL")
}

fn set_empty_dict_metatable(lua: &Lua, table: &Table) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let metatable: Table = vim.get("_empty_dict_mt")?;
    table.set_metatable(Some(metatable))
}
