//! Lua-facing compatibility tests for the Neovim standard-library bindings.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::rc::Rc;

use mlua::FromLuaMulti;
use ox_lua::{BuiltinHost, LuaHost, RuntimeRoot, Scheduler, Work};
use ox_types::{OxStr, Typval};

struct NoBuiltins;

impl BuiltinHost for NoBuiltins {
    fn call(&self, name: &OxStr, _args: Vec<Typval>) -> Result<Typval, String> {
        // The runtime prelude probes has('win32') during host init
        // (runtime/lua/vim/_core/system.lua).
        if name.as_bytes() == b"has" {
            return Ok(Typval::Number(0));
        }
        Err("builtin unavailable in stdlib test".to_owned())
    }
}

struct ImmediateScheduler;

impl Scheduler for ImmediateScheduler {
    fn schedule_deferred(&self, work: Work) -> Result<(), String> {
        work().map_err(|error| error.to_string())
    }
}

fn host() -> LuaHost {
    LuaHost::new(
        RuntimeRoot::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime")),
        Rc::new(NoBuiltins),
        Rc::new(ImmediateScheduler),
    )
    .unwrap()
}

fn eval<R: FromLuaMulti>(host: &LuaHost, source: &str) -> R {
    host.lua().load(source).eval().unwrap()
}

#[test]
fn mpack_uses_fixed_vectors_and_preserves_nil_binary_and_extensions() {
    let host = host();
    let nil: mlua::LuaString = eval(&host, "return vim.mpack.encode(vim.NIL)");
    assert!(eval::<bool>(&host, "return vim.mpack.NIL == vim.NIL"));
    assert_eq!(nil.as_bytes().as_ref(), [0xc0]);

    let array: mlua::LuaString = eval(&host, "return vim.mpack.encode({'a'})");
    assert_eq!(array.as_bytes().as_ref(), [0x91, 0xa1, b'a']);

    host.lua()
        .globals()
        .set("binary_value", host.lua().create_string([0, 255]).unwrap())
        .unwrap();
    let roundtrip: (bool, mlua::LuaString) = eval(
        &host,
        "local v = vim.mpack.decode(vim.mpack.encode({vim.NIL, binary_value})); \
         return v[1] == vim.NIL, v[2]",
    );
    assert!(roundtrip.0);
    assert_eq!(roundtrip.1.as_bytes().as_ref(), [0, 255]);

    host.lua()
        .globals()
        .set(
            "raw_ext",
            host.lua().create_string([0xd4, 42, 0xff]).unwrap(),
        )
        .unwrap();
    let extension: mlua::LuaString =
        eval(&host, "return vim.mpack.encode(vim.mpack.decode(raw_ext))");
    assert_eq!(extension.as_bytes().as_ref(), [0xd4, 42, 0xff]);
}

#[test]
fn mpack_distinguishes_arrays_maps_and_rejects_malformed_or_recursive_values() {
    let host = host();
    let vectors: (mlua::LuaString, mlua::LuaString) = eval(
        &host,
        "local dict = setmetatable({}, vim._empty_dict_mt); \
         return vim.mpack.encode({}), vim.mpack.encode(dict)",
    );
    assert_eq!(vectors.0.as_bytes().as_ref(), [0x90]);
    assert_eq!(vectors.1.as_bytes().as_ref(), [0x80]);

    host.lua()
        .globals()
        .set("malformed", host.lua().create_string([0x81]).unwrap())
        .unwrap();
    host.lua()
        .globals()
        .set("trailing", host.lua().create_string([0xc0, 0xc0]).unwrap())
        .unwrap();
    let errors: (bool, bool, bool) = eval(
        &host,
        "local recursive = {}; recursive.self = recursive; \
         local a = pcall(vim.mpack.decode, malformed); \
         local b = pcall(vim.mpack.decode, trailing); \
         local c = pcall(vim.mpack.encode, recursive); \
         return a, b, c",
    );
    assert_eq!(errors, (false, false, false));
}

#[test]
fn mpack_packer_and_unpacker_are_callable_streaming_sessions() {
    let host = host();
    let result: (bool, i64, i64, bool, i64, i64, bool, bool, bool) = eval(
        &host,
        "local pack = vim.mpack.Packer(); \
         local handlers = { [0] = function(kind, payload) \
           return kind == 0 and vim.mpack.decode(payload) \
         end }; \
         local unpack = vim.mpack.Unpacker({ ext = handlers }); \
         handlers[0] = function() return -1 end; \
         local encoded = pack({1, 'x'}); \
         local first, pos1 = unpack(encoded:sub(1, 2)); \
         local value, pos2 = unpack(encoded:sub(3)); \
         local one, pos3 = unpack(string.char(1, 2), 1); \
         local two, pos4 = unpack(string.char(1, 2), pos3); \
         local ext = unpack(string.char(0xd4, 0, 2)); \
         local raw_ext = vim.mpack.Unpacker()(string.char(0xd4, 7, 42)); \
         local mt = {}; \
         local ext_pack = vim.mpack.Packer({ \
           ext = { [mt] = function() return 7, string.char(42) end }, \
         }); \
         local packed_ext = ext_pack(setmetatable({}, mt)); \
         local packed_bin = vim.mpack.Packer({ is_bin = true })('x'); \
         return encoded == vim.mpack.encode({1, 'x'}), pos1, pos2, \
           first == nil and value[1] == 1 and value[2] == 'x' and ext == 2, \
           one + two, pos4, raw_ext == string.char(42), \
           packed_ext == string.char(0xd4, 7, 42), \
           packed_bin == string.char(0xc4, 1, string.byte('x'))",
    );

    assert_eq!(result, (true, 3, 3, true, 3, 3, true, true, true));
}

#[test]
fn mpack_session_frames_rpc_messages_and_correlates_responses() {
    let host = host();
    let result: (bool, bool, bool, bool, bool) = eval(
        &host,
        "local pack = vim.mpack.Packer(); \
         local session = vim.mpack.Session({ unpack = vim.mpack.Unpacker() }); \
         local callback = function() end; \
         local request = session:request(callback) .. pack('method') .. pack({3}); \
         local kind, id, method, args, request_pos = session:receive(request); \
         local response = session:reply(id) .. pack(vim.NIL) .. pack(7); \
         local rkind, registered, err, value, response_pos = session:receive(response); \
         local notification = session:notify() .. pack('event') .. pack({4}); \
         local first, _, _, _, fragment_pos = session:receive(notification:sub(1, 2)); \
         local nkind, none, event, event_args, notification_pos = \
           session:receive(notification:sub(3)); \
         return kind == 'request' and id == 0 and method == 'method' and args[1] == 3, \
           request_pos == #request + 1, \
           rkind == 'response' and registered == callback and err == vim.NIL and value == 7 \
             and response_pos == #response + 1, \
           first == nil and fragment_pos == 3, \
           nkind == 'notification' and none == nil and event == 'event' and event_args[1] == 4 \
             and notification_pos == #notification - 1",
    );

    assert_eq!(result, (true, true, true, true, true));
}

#[test]
fn mpack_encodes_nonfinite_numbers_with_upstream_float_widths() {
    let host = host();
    let encoded: (mlua::LuaString, mlua::LuaString, mlua::LuaString) = eval(
        &host,
        "return vim.mpack.encode(math.huge), vim.mpack.encode(-math.huge), \
                vim.mpack.encode(0 / 0)",
    );
    // conv.c's mpack_fits_single() keeps infinities in a 4-byte float because
    // (float)v == v holds, while NaN fails the check and takes 8 bytes.
    assert_eq!(
        encoded.0.as_bytes().as_ref(),
        [0xca, 0x7f, 0x80, 0x00, 0x00]
    );
    assert_eq!(
        encoded.1.as_bytes().as_ref(),
        [0xca, 0xff, 0x80, 0x00, 0x00]
    );
    let nan = encoded.2.as_bytes();
    assert_eq!(nan.len(), 9);
    assert_eq!(nan[0], 0xcb);

    let decoded: (bool, bool, bool, bool, bool) = eval(
        &host,
        "local inf = vim.mpack.decode(vim.mpack.encode(math.huge)); \
         local ninf = vim.mpack.decode(vim.mpack.encode(-math.huge)); \
         local nan = vim.mpack.decode(vim.mpack.encode(0 / 0)); \
         local f32 = vim.mpack.decode(string.char(0xca, 0x7f, 0x80, 0, 0)); \
         local f64 = vim.mpack.decode(string.char(0xcb, 0x7f, 0xf8, 0, 0, 0, 0, 0, 0)); \
         return inf == math.huge, ninf == -math.huge, nan ~= nan, \
           f32 == math.huge, f64 ~= f64",
    );
    assert_eq!(decoded, (true, true, true, true, true));
}

#[test]
fn mpack_encodes_numbers_matching_upstream_tokens_and_roundtrips() {
    let host = host();
    let encoded: (
        mlua::LuaString,
        mlua::LuaString,
        mlua::LuaString,
        mlua::LuaString,
        mlua::LuaString,
        mlua::LuaString,
        mlua::LuaString,
        mlua::LuaString,
    ) = eval(
        &host,
        "return vim.mpack.encode(1.5), \
                vim.mpack.encode(1.0), \
                vim.mpack.encode(-0.0), \
                vim.mpack.encode(2^63), \
                vim.mpack.encode(-1.0), \
                vim.mpack.encode(-2^63), \
                vim.mpack.encode(2^64 - 2048), \
                vim.mpack.encode(2^64)",
    );

    assert_eq!(encoded.0.as_bytes().as_ref(), [0xca, 0x3f, 0xc0, 0x00, 0x00]);
    assert_eq!(encoded.1.as_bytes().as_ref(), [0x01]);
    assert_eq!(encoded.2.as_bytes().as_ref(), [0x00]);
    assert_eq!(
        encoded.3.as_bytes().as_ref(),
        [0xcf, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
    assert_eq!(encoded.4.as_bytes().as_ref(), [0xff]);
    assert_eq!(
        encoded.5.as_bytes().as_ref(),
        [0xd3, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
    assert_eq!(
        encoded.6.as_bytes().as_ref(),
        [0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xf8, 0x00]
    );
    assert_eq!(encoded.7.as_bytes().as_ref(), [0xca, 0x5f, 0x80, 0x00, 0x00]);

    let roundtrips: (bool, bool, bool, bool, bool, bool, bool, bool) = eval(
        &host,
        "return vim.mpack.decode(vim.mpack.encode(1.5)) == 1.5, \
                vim.mpack.decode(vim.mpack.encode(1.0)) == 1.0, \
                vim.mpack.decode(vim.mpack.encode(-0.0)) == 0.0, \
                vim.mpack.decode(vim.mpack.encode(2^63)) == 2^63, \
                vim.mpack.decode(vim.mpack.encode(-1.0)) == -1.0, \
                vim.mpack.decode(vim.mpack.encode(-2^63)) == -2^63, \
                vim.mpack.decode(vim.mpack.encode(2^64 - 2048)) == 2^64 - 2048, \
                vim.mpack.decode(vim.mpack.encode(2^64)) == 2^64",
    );
    assert_eq!(roundtrips, (true, true, true, true, true, true, true, true));
}

#[test]
fn mpack_sessions_accept_optional_options_and_expose_header_only_receive() {
    let host = host();
    let result: (bool, bool, bool, bool, bool, bool, bool) = eval(
        &host,
        "local pack = vim.mpack.Packer(); \
         local ok_no_args = pcall(vim.mpack.Session); \
         local ok_nil = pcall(vim.mpack.Session, nil); \
         local ok_non_table = pcall(vim.mpack.Session, 'ignored'); \
         local ok_empty, empty_err = pcall(vim.mpack.Session, {}); \
         local ok_bad, bad_err = pcall(vim.mpack.Session, { unpack = 'nope' }); \
         local session = vim.mpack.Session('ignored'); \
         local callback = function() end; \
         local request = session:request(callback) .. pack('method') .. pack({3}); \
         local kind, id, request_pos = session:receive(request); \
         local request_ok = kind == 'request' and id == 0 and request_pos == 4; \
         local ok_payload = pcall(session.receive, session, request:sub(request_pos)); \
         local response = session:reply(id) .. pack(vim.NIL) .. pack(7); \
         local rkind, registered, response_pos = session:receive(response); \
         local response_ok = rkind == 'response' and registered == callback \
           and response_pos == 4; \
         local notification = session:notify() .. pack('event') .. pack({4}); \
         local nkind, none, notification_pos = session:receive(notification); \
         local notification_ok = nkind == 'notification' and none == nil \
           and notification_pos == 3; \
         local fresh = vim.mpack.Session(nil); \
         local first, second, fragment_pos = fresh:receive(request:sub(1, 2)); \
         local fkind, fid, resumed_pos = fresh:receive(request:sub(3)); \
         local fragment_ok = first == nil and second == nil and fragment_pos == 3 \
           and fkind == 'request' and fid == 0 and resumed_pos == 2; \
         return ok_no_args, ok_nil, ok_non_table, \
           not ok_empty and empty_err:find('unpack', 1, true) ~= nil, \
           not ok_bad and bad_err:find('unpack', 1, true) ~= nil, \
           request_ok, \
           not ok_payload and response_ok and notification_ok and fragment_ok",
    );
    assert_eq!(result, (true, true, true, true, true, true, true));
}

#[test]
fn mpack_session_receive_enforces_rpc_header_widths_in_both_modes() {
    let host = host();
    let result: (bool, bool, bool, bool, bool, bool) = eval(
        &host,
        "local pack = vim.mpack.Packer(); \
         local payload = pack('method') .. pack({3}); \
         local wide_kind = string.char(0x94, 0xcd, 0x00, 0x00, 0x00) .. payload; \
         local wide_id = string.char(0x94, 0x00, 0xcf, 0, 0, 0, 0, 0, 0, 0, 1) .. payload; \
         local u8_kind = string.char(0x94, 0xcc, 0x00, 0x00) .. payload; \
         local u32_id = string.char(0x94, 0x00, 0xce, 0, 0, 0, 5) .. payload; \
         local hdr_session = vim.mpack.Session(); \
         local unp_session = vim.mpack.Session({ unpack = vim.mpack.Unpacker() }); \
         local ok_hdr_wide_kind = pcall(hdr_session.receive, hdr_session, wide_kind); \
         local ok_unp_wide_kind = pcall(unp_session.receive, unp_session, wide_kind); \
         local ok_hdr_wide_id = pcall(hdr_session.receive, hdr_session, wide_id); \
         local ok_unp_wide_id = pcall(unp_session.receive, unp_session, wide_id); \
         local hk, hid, hpos = hdr_session:receive(u8_kind); \
         local uk, uid, umethod, uargs, upos = unp_session:receive(u8_kind); \
         local u8_ok = hk == 'request' and hid == 0 and hpos == 5 \
           and uk == 'request' and uid == 0 and umethod == 'method' and uargs[1] == 3 \
           and upos == #u8_kind + 1; \
         local hk32, hid32, hpos32 = hdr_session:receive(u32_id); \
         local uk32, uid32, umethod32, uargs32, upos32 = unp_session:receive(u32_id); \
         local u32_ok = hk32 == 'request' and hid32 == 5 and hpos32 == 8 \
           and uk32 == 'request' and uid32 == 5 and umethod32 == 'method' and uargs32[1] == 3 \
           and upos32 == #u32_id + 1; \
         return not ok_hdr_wide_kind, not ok_unp_wide_kind, \
           not ok_hdr_wide_id, not ok_unp_wide_id, \
           u8_ok, u32_ok",
    );
    assert_eq!(result, (true, true, true, true, true, true));
}

#[test]
fn json_encode_covers_shape_formatting_and_scalar_boundaries() {
    let host = host();
    let encoded: (String, String, String, String) = eval(
        &host,
        "local dict = setmetatable({}, vim._empty_dict_mt); \
         return vim.json.encode({}), vim.json.encode(dict), \
           vim.json.encode({b=2,a=1}, {sort_keys=true, indent='\t'}), \
           vim.json.encode({path='a/b'}, {escape_slash=true})",
    );
    assert_eq!(encoded.0, "[]");
    assert_eq!(encoded.1, "{}");
    assert_eq!(encoded.2, "{\n\t\"a\": 1,\n\t\"b\": 2\n}");
    assert_eq!(encoded.3, r#"{"path":"a\/b"}"#);

    host.lua()
        .globals()
        .set("exact_i64", 9_007_199_254_740_991_i64)
        .unwrap();
    let scalars: (String, String, String, String) = eval(
        &host,
        "return vim.json.encode(vim.NIL), vim.json.encode(true), \
         vim.json.encode(exact_i64), vim.json.encode('é')",
    );
    assert_eq!(
        scalars,
        (
            "null".into(),
            "true".into(),
            "9007199254740991".into(),
            "\"é\"".into()
        )
    );
}

#[test]
fn json_decode_honors_luanil_comments_and_empty_container_identity() {
    let host = host();
    let result: (bool, bool, bool, bool, i64) = eval(
        &host,
        "local value = vim.json.decode('{/*x*/\"a\":null,\"b\":[null],\"c\":{},\"d\":[]}', \
           {skip_comments=true, luanil={object=true,array=true}}); \
         return value.a == nil, value.b[1] == nil, \
           getmetatable(value.c) == vim._empty_dict_mt, \
           getmetatable(value.d) == nil, #value.d",
    );
    assert_eq!(result, (true, true, true, true, 0));

    let sentinel: bool = eval(&host, "return vim.json.decode('null') == vim.NIL");
    assert!(sentinel);
}

#[test]
fn json_rejects_invalid_options_values_and_shapes() {
    let host = host();
    let errors: (bool, bool, bool, bool, bool) = eval(
        &host,
        "local recursive = {}; recursive.self = recursive; \
         local sparse = {[1]='a',[3]='c'}; \
         local a = pcall(vim.json.decode, '{bad}'); \
         local b = pcall(vim.json.decode, '/*', {skip_comments=true}); \
         local c = pcall(vim.json.encode, 0/0); \
         local d = pcall(vim.json.encode, sparse); \
         local e = pcall(vim.json.encode, recursive); \
         return a,b,c,d,e",
    );
    assert_eq!(errors, (false, false, false, false, false));
}

#[test]
fn diff_matches_neovim_unified_and_index_oracles() {
    let host = host();
    let unified: String = eval(&host, "return vim.diff('a\\nb\\nc\\n','a\\nx\\nc\\n',{})");
    assert_eq!(unified, "@@ -2 +2 @@\n-b\n+x\n");

    let indices: (i64, i64, i64, i64) = eval(
        &host,
        "local h=vim.diff('a\\nb\\nc\\n','a\\nx\\nc\\n',{result_type='indices'})[1]; \
         return h[1],h[2],h[3],h[4]",
    );
    assert_eq!(indices, (2, 1, 2, 1));

    let no_newline: String = eval(&host, "return vim.diff('one\\ntwo\\n','one\\ntwo',{})");
    assert_eq!(
        no_newline,
        "@@ -2 +2 @@\n-two\n+two\n\\ No newline at end of file\n"
    );

    let histogram: (i64, i64, i64, i64) = eval(
        &host,
        "local h=vim.diff('a\\nb\\nc\\nd\\n','a\\nc\\nd\\n', \
          {algorithm='histogram',result_type='indices'})[1]; return h[1],h[2],h[3],h[4]",
    );
    assert_eq!(histogram, (2, 1, 1, 0));
}

#[test]
fn diff_supports_context_callbacks_and_option_errors() {
    let host = host();
    let result: (bool, i64, i64, i64, i64) = eval(
        &host,
        "local seen; local result=vim.diff('a\\nb\\n','a\\nx\\n',{on_hunk=function(...) seen={...}; return -1 end}); \
         return result == nil, seen[1],seen[2],seen[3],seen[4]",
    );
    assert_eq!(result, (true, 2, 1, 2, 1));
    let errors: (bool, bool, bool) = eval(
        &host,
        "return pcall(vim.diff,'a','b',{algorithm='bogus'}), \
          pcall(vim.diff,'a','b',{result_type='bogus'}), \
          pcall(vim.diff,'a','b',{ctxlen=-1})",
    );
    assert_eq!(errors, (false, false, false));
}

#[test]
fn vim_text_lazy_ownership_loads_once_and_exposes_functions() {
    let host = host();

    // Before first access, vim.diff is installed but vim.text is not eagerly
    // created by Rust. The lazy loader in _init_packages owns vim.text.
    let pre: (String, bool) = eval(&host, "return type(vim.diff), rawget(vim, 'text') == nil");
    assert_eq!(pre, ("function".to_owned(), true));

    // First access loads the embedded vim.text module exactly once.
    let is_required: bool = eval(&host, "return vim.text == require('vim.text')");
    assert!(is_required);

    // Repeated access returns the same table identity (loaded once, cached).
    let same_identity: bool = eval(&host, "return vim.text == vim.text");
    assert!(same_identity);

    // indent, hexencode, hexdecode, and diff are all functions on vim.text.
    let ftypes: (String, String, String, String) = eval(
        &host,
        "return type(vim.text.indent), type(vim.text.hexencode), \
         type(vim.text.hexdecode), type(vim.text.diff)",
    );
    assert_eq!(
        ftypes,
        (
            "function".to_owned(),
            "function".to_owned(),
            "function".to_owned(),
            "function".to_owned(),
        ),
    );

    // vim.text.diff delegates to vim.diff; both return the same result.
    let diff_agree: (String, String) = eval(
        &host,
        "return vim.diff('a\\nb\\nc\\n','a\\nx\\nc\\n',{}), \
         vim.text.diff('a\\nb\\nc\\n','a\\nx\\nc\\n',{})",
    );
    assert_eq!(diff_agree.0, diff_agree.1);
    assert_eq!(diff_agree.0, "@@ -2 +2 @@\n-b\n+x\n");

    // One indent and hex round-trip through the embedded module.
    let indent: (String, i64) = eval(&host, "return vim.text.indent(2, 'a\\nb\\n')");
    assert_eq!(indent.0, "  a\n  b\n");
    assert_eq!(indent.1, 0);

    let hex_roundtrip: (String, String) = eval(
        &host,
        "local s = 'Hello'; return vim.text.hexencode(s), vim.text.hexdecode(vim.text.hexencode(s))",
    );
    assert_eq!(hex_roundtrip.1, "Hello");
    assert_eq!(hex_roundtrip.0, "48656C6C6F");
}

#[test]
fn base64_matches_rfc_vectors_binary_boundaries_and_errors() {
    let host = host();
    let vectors: (String, String, String, String, mlua::LuaString) = eval(
        &host,
        "return vim.base64.encode(''), vim.base64.encode('f'), vim.base64.encode('fo'), \
         vim.base64.encode('foo'), vim.base64.decode('AP+A')",
    );
    assert_eq!(vectors.0, "");
    assert_eq!(vectors.1, "Zg==");
    assert_eq!(vectors.2, "Zm8=");
    assert_eq!(vectors.3, "Zm9v");
    assert_eq!(vectors.4.as_bytes().as_ref(), [0, 255, 128]);
    let errors: (bool, bool, bool, bool) = eval(
        &host,
        "return pcall(vim.base64.decode,'Zg='), pcall(vim.base64.decode,'Z===') , \
          pcall(vim.base64.decode,'Zh=='), pcall(vim.base64.decode,'Zm$=')",
    );
    assert_eq!(errors, (false, false, false, false));
}

#[test]
fn regex_reports_byte_spans_no_match_and_line_relative_ranges() {
    let host = host();
    let spans: (i64, i64, bool) = eval(
        &host,
        "local re=vim.regex('é'); local s,e=re:match_str('aéz'); \
         local x,y=re:match_str('abc'); return s,e,x==nil and y==nil",
    );
    assert_eq!(spans, (1, 3, true));

    let line_span: (i64, i64) = eval(
        &host,
        "vim.api.nvim_buf_get_lines=function(buf,s,e,strict) return {'xxéyy'} end; \
         local s,e=vim.regex('é'):match_line(7,0,1,5); return s,e",
    );
    assert_eq!(line_span, (1, 3));
}

#[test]
fn regex_rejects_invalid_patterns_ranges_and_utf8() {
    let host = host();
    host.lua()
        .globals()
        .set("invalid_utf8", host.lua().create_string([0xff]).unwrap())
        .unwrap();
    let errors: (bool, bool, bool, bool) = eval(
        &host,
        "vim.api.nvim_buf_get_lines=function() return {'abc'} end; \
         local re=vim.regex('a'); \
         return pcall(vim.regex,'['), pcall(re.match_str,re,invalid_utf8), \
           pcall(re.match_line,re,0,-1), pcall(re.match_line,re,0,0,3,2)",
    );
    assert_eq!(errors, (false, false, false, false));
}

#[test]
fn lpeg_exposes_vim_spec_surface_and_shared_module_identity() {
    let host = host();
    // test/functional/lua/vim_spec.lua "lpeg"
    assert_eq!(
        eval::<i64>(&host, "return vim.lpeg.match(vim.lpeg.R'09'^1, '4504ab')"),
        5
    );
    // stdlib.c registers the same table as vim.lpeg and package.loaded.lpeg.
    assert!(eval::<bool>(&host, "return require('lpeg') == vim.lpeg"));
    // vim.glob relies on the pattern metatable arithmetic metamethods.
    assert!(eval::<bool>(
        &host,
        "return type(getmetatable(vim.lpeg.P(1)).__add) == 'function'"
    ));
    assert!(eval::<bool>(
        &host,
        "return vim.lpeg.version == 'LPeg 1.1.0'"
    ));
}

#[test]
fn lpeg_passes_upstream_lpeg_1_1_0_test_suite() {
    let host = host();
    let lua = host.lua();
    let lpeg_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/lpeg");
    // Upstream test.lua does `require "re"` for the regex-like interface;
    // preload the vendored re.lua under that name.
    let re_source = std::fs::read_to_string(lpeg_dir.join("re.lua")).unwrap();
    let re_loader = lua.load(re_source).into_function().unwrap();
    let package: mlua::Table = lua.globals().get("package").unwrap();
    let preload: mlua::Table = package.get("preload").unwrap();
    preload.set("re", re_loader).unwrap();

    let test_source = std::fs::read_to_string(lpeg_dir.join("test.lua")).unwrap();
    lua.load(test_source)
        .set_name("=third_party/lpeg/test.lua")
        .exec()
        .unwrap();
}
