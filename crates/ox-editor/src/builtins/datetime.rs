//! Date and time formatting builtins (`strftime`).

use std::fmt::Write as _;
use std::path::PathBuf;

use ox_eval::EvalError;
use ox_types::{OxStr, Typval};

const ABBR_WDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const FULL_WDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const ABBR_MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const FULL_MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const MIN_REPRESENTABLE_TIMESTAMP: i64 = -67_768_040_609_771_272;
const MAX_REPRESENTABLE_TIMESTAMP: i64 = 67_768_036_191_644_399;
const MAX_RESULT_LEN: usize = 255;

#[derive(Clone, Debug, Eq, PartialEq)]
struct TimeZoneInfo {
    offset: i64,
    name: String,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Pad {
    Default,
    None,
    Space,
    Zero,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DateTimeParts {
    year: i64,
    month: u32,
    month_day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    wday: u32,
    yday: u32,
    u_week: u32,
    w_week: u32,
    iso_year: i64,
    iso_week: u32,
}

impl DateTimeParts {
    fn from_local_seconds(local_sec: i64) -> Option<Self> {
        let day = local_sec.div_euclid(86_400);
        let sec_of_day = local_sec.rem_euclid(86_400);
        let (year, month, month_day) = civil_from_days(day);

        let year_min = i64::from(i32::MIN) + 1900;
        let year_max = i64::from(i32::MAX) + 1900;
        if year < year_min || year > year_max {
            return None;
        }

        let hour = u32::try_from(sec_of_day / 3600).ok()?;
        let minute = u32::try_from((sec_of_day % 3600) / 60).ok()?;
        let second = u32::try_from(sec_of_day % 60).ok()?;
        let wday = u32::try_from((day + 4).rem_euclid(7)).ok()?;
        let jan1 = days_from_civil(year, 1, 1);
        let yday = u32::try_from(day - jan1 + 1).ok()?;

        let sunday_offset = i64::from(yday) - 1 - i64::from(wday);
        let u_week = u32::try_from(if sunday_offset < 0 { 0 } else { (sunday_offset / 7) + 1 }).ok()?;

        let mon_wday = (wday + 6) % 7;
        let monday_offset = i64::from(yday) - 1 - i64::from(mon_wday);
        let w_week = u32::try_from(if monday_offset < 0 { 0 } else { (monday_offset / 7) + 1 }).ok()?;

        let thu_day = day + 3 - i64::from(mon_wday);
        let (iso_year, _, _) = civil_from_days(thu_day);
        let iso_jan1 = days_from_civil(iso_year, 1, 1);
        let iso_week = u32::try_from((thu_day - iso_jan1) / 7 + 1).ok()?;

        Some(Self {
            year,
            month,
            month_day,
            hour,
            minute,
            second,
            wday,
            yday,
            u_week,
            w_week,
            iso_year,
            iso_week,
        })
    }
}

/// Formats date and time according to format string and optional timestamp.
pub(crate) fn call_strftime_builtin(args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: strftime",
        ));
    }
    if args.len() > 2 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: strftime",
        ));
    }

    let format_str = super::input_string_arg(&args[0])?;
    let format = format_str.to_string_lossy();
    let timestamp = extract_timestamp(args.get(1))?;

    if !(MIN_REPRESENTABLE_TIMESTAMP..=MAX_REPRESENTABLE_TIMESTAMP).contains(&timestamp) {
        return Ok(Typval::String(OxStr::from("(Invalid)")));
    }

    let tz = resolve_timezone(timestamp);
    let Some(local_sec) = timestamp.checked_add(tz.offset) else {
        return Ok(Typval::String(OxStr::from("(Invalid)")));
    };

    let Some(parts) = DateTimeParts::from_local_seconds(local_sec) else {
        return Ok(Typval::String(OxStr::from("(Invalid)")));
    };

    let formatted = format_strftime(&format, timestamp, &parts, &tz);

    Ok(Typval::String(OxStr::from(formatted.as_str())))
}

fn extract_timestamp(time_arg: Option<&Typval>) -> ox_eval::Result<i64> {
    if let Some(arg) = time_arg {
        super::position::number_value(arg)
    } else {
        Ok(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            }))
    }
}

fn resolve_timezone(timestamp: i64) -> TimeZoneInfo {
    if let Ok(tz_env) = std::env::var("TZ") {
        if !tz_env.is_empty() {
            if let Some(info) = resolve_tz_string(&tz_env, timestamp) {
                return info;
            }
        }
    }

    if let Ok(data) = std::fs::read("/etc/localtime") {
        if let Some(info) = parse_tzif(&data, timestamp) {
            return info;
        }
    }

    TimeZoneInfo {
        offset: 0,
        name: "UTC".to_string(),
    }
}

fn resolve_tz_string(tz: &str, timestamp: i64) -> Option<TimeZoneInfo> {
    if tz == "UTC" || tz == "GMT" || tz == "UTC0" || tz == "GMT0" {
        return Some(TimeZoneInfo {
            offset: 0,
            name: tz.to_string(),
        });
    }

    let path = if let Some(stripped) = tz.strip_prefix(':') {
        if stripped.starts_with('/') {
            PathBuf::from(stripped)
        } else {
            PathBuf::from("/usr/share/zoneinfo").join(stripped)
        }
    } else {
        PathBuf::from("/usr/share/zoneinfo").join(tz)
    };

    if let Ok(data) = std::fs::read(&path) {
        if let Some(info) = parse_tzif(&data, timestamp) {
            return Some(info);
        }
    }

    parse_posix_tz(tz, timestamp)
}

fn parse_posix_tz(tz: &str, timestamp: i64) -> Option<TimeZoneInfo> {
    let bytes = tz.as_bytes();
    let mut cursor = 0;

    let std_name = parse_tz_name(bytes, &mut cursor)?;
    let std_offset = parse_tz_offset(bytes, &mut cursor);

    if cursor >= bytes.len() || bytes.get(cursor) == Some(&b',') {
        return Some(TimeZoneInfo {
            offset: std_offset,
            name: std_name.to_string(),
        });
    }

    let dst_name = match parse_tz_name(bytes, &mut cursor) {
        Some(name) => name,
        None => {
            return Some(TimeZoneInfo {
                offset: std_offset,
                name: std_name.to_string(),
            });
        }
    };

    let dst_offset = if cursor < bytes.len()
        && matches!(bytes.get(cursor), Some(b'+' | b'-' | b'0'..=b'9'))
    {
        parse_tz_offset(bytes, &mut cursor)
    } else {
        std_offset + 3600
    };

    if let Ok(data) = std::fs::read("/usr/share/zoneinfo/posixrules") {
        if let Some(info) = parse_tzif(&data, timestamp) {
            let name = if info.offset == dst_offset {
                dst_name.to_string()
            } else {
                std_name.to_string()
            };
            return Some(TimeZoneInfo {
                offset: info.offset,
                name,
            });
        }
    }

    Some(TimeZoneInfo {
        offset: std_offset,
        name: std_name.to_string(),
    })
}

fn parse_tz_name<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a str> {
    if bytes.get(*cursor) == Some(&b'<') {
        let start = *cursor + 1;
        let end = bytes.get(start..)?.iter().position(|&b| b == b'>')? + start;
        *cursor = end + 1;
        std::str::from_utf8(bytes.get(start..end)?).ok()
    } else {
        let start = *cursor;
        while *cursor < bytes.len() && bytes.get(*cursor).is_some_and(u8::is_ascii_alphabetic) {
            *cursor += 1;
        }
        (*cursor > start).then(|| std::str::from_utf8(bytes.get(start..*cursor)?).ok())?
    }
}

fn parse_tz_offset(bytes: &[u8], cursor: &mut usize) -> i64 {
    if *cursor >= bytes.len() {
        return 0;
    }
    let sign: i64 = if bytes.get(*cursor) == Some(&b'-') {
        *cursor += 1;
        1
    } else {
        if bytes.get(*cursor) == Some(&b'+') {
            *cursor += 1;
        }
        -1
    };

    let hours = parse_digits(bytes, cursor);
    let mins = parse_colon_digits(bytes, cursor);
    let secs = parse_colon_digits(bytes, cursor);

    sign * (hours * 3600 + mins * 60 + secs)
}

fn parse_digits(bytes: &[u8], cursor: &mut usize) -> i64 {
    let mut value = 0_i64;
    while *cursor < bytes.len() && bytes.get(*cursor).is_some_and(u8::is_ascii_digit) {
        if let Some(b) = bytes.get(*cursor) {
            value = value * 10 + i64::from(b - b'0');
        }
        *cursor += 1;
    }
    value
}

fn parse_colon_digits(bytes: &[u8], cursor: &mut usize) -> i64 {
    if bytes.get(*cursor) == Some(&b':') {
        *cursor += 1;
        parse_digits(bytes, cursor)
    } else {
        0
    }
}

fn parse_tzif(data: &[u8], timestamp: i64) -> Option<TimeZoneInfo> {
    if data.len() < 44 || data.get(0..4) != Some(b"TZif") {
        return None;
    }

    if let Some(pos) = data.get(4..)?.windows(4).position(|w| w == b"TZif") {
        let v2_start = 4 + pos;
        if data.len().saturating_sub(v2_start) >= 44 {
            if let Some(v2_block) = data.get(v2_start..) {
                return parse_tzif_v2(v2_block, timestamp);
            }
        }
    }

    parse_tzif_v1(data, timestamp)
}

fn parse_tzif_v1(data: &[u8], timestamp: i64) -> Option<TimeZoneInfo> {
    parse_tzif_data(data, timestamp, 4, |slice| {
        Some(i64::from(i32::from_be_bytes(slice.try_into().ok()?)))
    })
}

fn parse_tzif_v2(data: &[u8], timestamp: i64) -> Option<TimeZoneInfo> {
    parse_tzif_data(data, timestamp, 8, |slice| {
        Some(i64::from_be_bytes(slice.try_into().ok()?))
    })
}

fn parse_tzif_data<F>(data: &[u8], timestamp: i64, time_elem_size: usize, read_time: F) -> Option<TimeZoneInfo>
where
    F: Fn(&[u8]) -> Option<i64>,
{
    if data.len() < 44 {
        return None;
    }
    let timecnt = usize::try_from(u32::from_be_bytes(data.get(32..36)?.try_into().ok()?)).ok()?;
    let typecnt = usize::try_from(u32::from_be_bytes(data.get(36..40)?.try_into().ok()?)).ok()?;
    let charcnt = usize::try_from(u32::from_be_bytes(data.get(40..44)?.try_into().ok()?)).ok()?;

    let times_len = timecnt.checked_mul(time_elem_size)?;
    let types_len = timecnt;
    let ttinfo_len = typecnt.checked_mul(6)?;

    let offset = 44_usize;
    let times_end = offset.checked_add(times_len)?;
    let types_end = times_end.checked_add(types_len)?;
    let ttinfo_end = types_end.checked_add(ttinfo_len)?;
    let char_end = ttinfo_end.checked_add(charcnt)?;

    if data.len() < char_end {
        return None;
    }

    let times_slice = data.get(offset..times_end)?;
    let types_slice = data.get(times_end..types_end)?;
    let ttinfo_slice = data.get(types_end..ttinfo_end)?;
    let abbrs_slice = data.get(ttinfo_end..char_end)?;

    let mut ttinfos = Vec::with_capacity(typecnt);
    for chunk in ttinfo_slice.chunks_exact(6) {
        let utoff = i32::from_be_bytes(chunk.get(0..4)?.try_into().ok()?);
        let is_dst = chunk.get(4).copied()? != 0;
        let desigidx = usize::from(chunk.get(5).copied()?);
        ttinfos.push((utoff, is_dst, desigidx));
    }
    if ttinfos.is_empty() {
        return None;
    }

    let selected_type = select_transition_type(timecnt, times_slice, types_slice, &ttinfos, time_elem_size, timestamp, &read_time);
    let (utoff, _, desigidx) = ttinfos.get(selected_type).copied().unwrap_or(ttinfos[0]);

    let abbr = if desigidx < abbrs_slice.len() {
        let end = abbrs_slice[desigidx..]
            .iter()
            .position(|&b| b == 0)
            .map_or(abbrs_slice.len(), |pos| desigidx + pos);
        String::from_utf8_lossy(&abbrs_slice[desigidx..end]).into_owned()
    } else {
        "UTC".to_string()
    };

    Some(TimeZoneInfo {
        offset: i64::from(utoff),
        name: abbr,
    })
}

fn select_transition_type<F>(
    timecnt: usize,
    times_slice: &[u8],
    types_slice: &[u8],
    ttinfos: &[(i32, bool, usize)],
    time_elem_size: usize,
    timestamp: i64,
    read_time: &F,
) -> usize
where
    F: Fn(&[u8]) -> Option<i64>,
{
    if timecnt == 0 {
        return 0;
    }
    let Some(first_time) = times_slice.get(0..time_elem_size).and_then(read_time) else {
        return 0;
    };

    if timestamp < first_time {
        return ttinfos.iter().position(|t| !t.1).unwrap_or(0);
    }

    let mut low = 0;
    let mut high = timecnt;
    while low < high {
        let mid = (low + high) / 2;
        let start = mid * time_elem_size;
        let mid_time = times_slice.get(start..start + time_elem_size).and_then(read_time).unwrap_or(0);
        if mid_time <= timestamp {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    let idx = low.saturating_sub(1);
    usize::from(types_slice.get(idx).copied().unwrap_or(0))
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let mp = u64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let civil = i64::try_from(doe).unwrap_or(0);
    era * 146097 + civil - 719468
}

fn civil_from_days(day: i64) -> (i64, u32, u32) {
    let z = day + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (i64::try_from(yoe).unwrap_or(0)) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = y + i64::from(m <= 2);
    (year, m, d)
}

fn format_strftime(
    format: &str,
    timestamp: i64,
    parts: &DateTimeParts,
    tz: &TimeZoneInfo,
) -> String {
    let mut out = String::with_capacity(format.len() * 2);
    let bytes = format.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes.get(i) == Some(&b'%') {
            i += 1;
            if i >= bytes.len() {
                out.push('%');
                break;
            }

            let pad = match bytes.get(i) {
                Some(b'-') => {
                    i += 1;
                    Pad::None
                }
                Some(b'_') => {
                    i += 1;
                    Pad::Space
                }
                Some(b'0') => {
                    i += 1;
                    Pad::Zero
                }
                Some(b'E' | b'O') => {
                    i += 1;
                    Pad::Default
                }
                _ => Pad::Default,
            };

            if i >= bytes.len() {
                out.push('%');
                break;
            }

            let spec = bytes.get(i).copied().unwrap_or(b'%');
            i += 1;

            if !format_directive(&mut out, spec, pad, parts, timestamp, tz) {
                out.push('%');
                match pad {
                    Pad::None => out.push('-'),
                    Pad::Space => out.push('_'),
                    Pad::Zero => out.push('0'),
                    Pad::Default => {}
                }
                out.push(char::from(spec));
            }
        } else {
            let next_pct = bytes[i..]
                .iter()
                .position(|&b| b == b'%')
                .map_or(bytes.len(), |pos| i + pos);
            if let Some(slice) = bytes.get(i..next_pct) {
                out.push_str(&String::from_utf8_lossy(slice));
            }
            i = next_pct;
        }
    }

    if out.len() > MAX_RESULT_LEN {
        return String::new();
    }
    out
}

fn format_directive(
    out: &mut String,
    spec: u8,
    pad: Pad,
    parts: &DateTimeParts,
    timestamp: i64,
    tz: &TimeZoneInfo,
) -> bool {
    format_date_directive(out, spec, pad, parts)
        || format_time_directive(out, spec, pad, parts)
        || format_week_directive(out, spec, pad, parts)
        || format_zone_directive(out, spec, parts, timestamp, tz)
}

fn format_date_directive(out: &mut String, spec: u8, pad: Pad, parts: &DateTimeParts) -> bool {
    match spec {
        b'%' => out.push('%'),
        b'n' => out.push('\n'),
        b't' => out.push('\t'),
        b'Y' => format_num(out, parts.year, 4, pad),
        b'y' => format_num(out, parts.year.abs() % 100, 2, pad),
        b'C' => format_num(out, parts.year / 100, 2, pad),
        b'm' => format_num(out, parts.month, 2, pad),
        b'B' => out.push_str(FULL_MONTHS.get(parts.month.saturating_sub(1) as usize).copied().unwrap_or("")),
        b'b' | b'h' => out.push_str(ABBR_MONTHS.get(parts.month.saturating_sub(1) as usize).copied().unwrap_or("")),
        b'd' => format_num(out, parts.month_day, 2, pad),
        b'e' => format_num(
            out,
            parts.month_day,
            2,
            if pad == Pad::Default { Pad::Space } else { pad },
        ),
        b'F' => {
            let _ = write!(out, "{:04}-{:02}-{:02}", parts.year, parts.month, parts.month_day);
        }
        b'D' => {
            let _ = write!(out, "{:02}/{:02}/{:02}", parts.month, parts.month_day, parts.year.abs() % 100);
        }
        _ => return false,
    }
    true
}

fn format_time_directive(out: &mut String, spec: u8, pad: Pad, parts: &DateTimeParts) -> bool {
    let hour12 = match parts.hour % 12 {
        0 => 12,
        h => h,
    };
    let is_pm = parts.hour >= 12;

    match spec {
        b'H' => format_num(out, parts.hour, 2, pad),
        b'k' => format_num(
            out,
            parts.hour,
            2,
            if pad == Pad::Default { Pad::Space } else { pad },
        ),
        b'I' => format_num(out, hour12, 2, pad),
        b'l' => format_num(
            out,
            hour12,
            2,
            if pad == Pad::Default { Pad::Space } else { pad },
        ),
        b'M' => format_num(out, parts.minute, 2, pad),
        b'S' => format_num(out, parts.second, 2, pad),
        b'p' => out.push_str(if is_pm { "PM" } else { "AM" }),
        b'P' => out.push_str(if is_pm { "pm" } else { "am" }),
        b'r' => {
            let _ = write!(
                out,
                "{hour12:02}:{:02}:{:02} {}",
                parts.minute,
                parts.second,
                if is_pm { "PM" } else { "AM" }
            );
        }
        b'R' => {
            let _ = write!(out, "{:02}:{:02}", parts.hour, parts.minute);
        }
        b'T' => {
            let _ = write!(out, "{:02}:{:02}:{:02}", parts.hour, parts.minute, parts.second);
        }
        _ => return false,
    }
    true
}

fn format_week_directive(out: &mut String, spec: u8, pad: Pad, parts: &DateTimeParts) -> bool {
    let u_wday = if parts.wday == 0 { 7 } else { parts.wday };
    match spec {
        b'a' => out.push_str(ABBR_WDAYS.get(parts.wday as usize).copied().unwrap_or("")),
        b'A' => out.push_str(FULL_WDAYS.get(parts.wday as usize).copied().unwrap_or("")),
        b'u' => {
            let _ = write!(out, "{u_wday}");
        }
        b'w' => {
            let _ = write!(out, "{}", parts.wday);
        }
        b'j' => format_num(out, parts.yday, 3, pad),
        b'U' => format_num(out, parts.u_week, 2, pad),
        b'W' => format_num(out, parts.w_week, 2, pad),
        b'V' => format_num(out, parts.iso_week, 2, pad),
        b'G' => {
            let _ = write!(out, "{:04}", parts.iso_year);
        }
        b'g' => {
            let _ = write!(out, "{:02}", parts.iso_year.abs() % 100);
        }
        _ => return false,
    }
    true
}

fn format_zone_directive(
    out: &mut String,
    spec: u8,
    parts: &DateTimeParts,
    timestamp: i64,
    tz: &TimeZoneInfo,
) -> bool {
    let hour12 = match parts.hour % 12 {
        0 => 12,
        h => h,
    };
    let is_pm = parts.hour >= 12;

    match spec {
        b's' => {
            let _ = write!(out, "{timestamp}");
        }
        b'z' => {
            let off_sign = if tz.offset >= 0 { '+' } else { '-' };
            let off_abs = tz.offset.abs();
            let off_h = off_abs / 3600;
            let off_m = (off_abs % 3600) / 60;
            let _ = write!(out, "{off_sign}{off_h:02}{off_m:02}");
        }
        b'Z' => out.push_str(&tz.name),
        b'c' => {
            let _ = write!(
                out,
                "{} {:02} {} {:04} {:02}:{:02}:{:02} {} {}",
                ABBR_WDAYS.get(parts.wday as usize).copied().unwrap_or(""),
                parts.month_day,
                ABBR_MONTHS.get(parts.month.saturating_sub(1) as usize).copied().unwrap_or(""),
                parts.year,
                hour12,
                parts.minute,
                parts.second,
                if is_pm { "PM" } else { "AM" },
                tz.name
            );
        }
        b'x' => {
            let _ = write!(out, "{:02}/{:02}/{:04}", parts.month, parts.month_day, parts.year);
        }
        b'X' => {
            let _ = write!(
                out,
                "{hour12:02}:{:02}:{:02} {}",
                parts.minute,
                parts.second,
                if is_pm { "PM" } else { "AM" }
            );
        }
        _ => return false,
    }
    true
}

fn format_num<T: std::fmt::Display + Copy>(
    out: &mut String,
    val: T,
    width: usize,
    pad: Pad,
) {
    let s = val.to_string();
    if pad == Pad::None {
        out.push_str(&s);
        return;
    }

    let (is_negative, digits) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else {
        (false, s.as_str())
    };

    let total_len = if is_negative { digits.len() + 1 } else { digits.len() };
    let pad_char = if pad == Pad::Space { ' ' } else { '0' };

    if pad == Pad::Space && is_negative {
        for _ in total_len..width {
            out.push(' ');
        }
        out.push('-');
        out.push_str(digits);
    } else if pad == Pad::Zero && is_negative {
        out.push('-');
        for _ in total_len..width {
            out.push('0');
        }
        out.push_str(digits);
    } else {
        for _ in total_len..width {
            out.push(pad_char);
        }
        out.push_str(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_types::OxStr;

    fn strftime_utc(fmt: &str, timestamp: i64) -> String {
        let tz = TimeZoneInfo {
            offset: 0,
            name: "UTC".to_string(),
        };
        let parts = DateTimeParts::from_local_seconds(timestamp).unwrap();
        format_strftime(fmt, timestamp, &parts, &tz)
    }

    fn strftime_tz(fmt: &str, timestamp: i64, offset: i64, name: &str) -> String {
        let tz = TimeZoneInfo {
            offset,
            name: name.to_string(),
        };
        let local = timestamp + offset;
        let parts = DateTimeParts::from_local_seconds(local).unwrap();
        format_strftime(fmt, timestamp, &parts, &tz)
    }

    #[test]
    fn strftime_epoch_and_fixed_times() {
        assert_eq!(strftime_utc("%Y-%m-%d", 0), "1970-01-01");
        assert_eq!(strftime_utc("%H:%M:%S", 0), "00:00:00");
        assert_eq!(strftime_utc("%Y-%m-%d %H:%M:%S", 0), "1970-01-01 00:00:00");
        assert_eq!(strftime_utc("%H:%M:%S", 86461), "00:01:01");
        assert_eq!(strftime_utc("%Y-%m-%d", 86461), "1970-01-02");
        assert_eq!(strftime_utc("%s", 1234567890), "1234567890");
        assert_eq!(strftime_utc("%s", 0), "0");
    }

    #[test]
    fn strftime_timezones() {
        assert_eq!(strftime_utc("%z", 0), "+0000");
        assert_eq!(strftime_utc("%Z", 0), "UTC");
        assert_eq!(
            strftime_tz("%Y-%m-%d %H:%M:%S %z %Z", 0, 32400, "KST"),
            "1970-01-01 09:00:00 +0900 KST"
        );
        assert_eq!(
            strftime_tz("%Y-%m-%d %H:%M:%S %z %Z", 0, -18000, "EST"),
            "1969-12-31 19:00:00 -0500 EST"
        );
    }

    #[test]
    fn strftime_posix_directives() {
        assert_eq!(strftime_utc("%C", 0), "19");
        assert_eq!(strftime_utc("%y", 0), "70");
        assert_eq!(strftime_utc("%m", 0), "01");
        assert_eq!(strftime_utc("%d", 0), "01");
        assert_eq!(strftime_utc("%e", 0), " 1");
        assert_eq!(strftime_utc("%H", 0), "00");
        assert_eq!(strftime_utc("%k", 0), " 0");
        assert_eq!(strftime_utc("%I", 0), "12");
        assert_eq!(strftime_utc("%l", 0), "12");
        assert_eq!(strftime_utc("%M", 0), "00");
        assert_eq!(strftime_utc("%S", 0), "00");
        assert_eq!(strftime_utc("%p", 0), "AM");
        assert_eq!(strftime_utc("%P", 0), "am");
        assert_eq!(strftime_utc("%F", 0), "1970-01-01");
        assert_eq!(strftime_utc("%T", 0), "00:00:00");
        assert_eq!(strftime_utc("%R", 0), "00:00");
        assert_eq!(strftime_utc("%r", 0), "12:00:00 AM");
        assert_eq!(strftime_utc("%D", 0), "01/01/70");
        assert_eq!(strftime_utc("%a", 0), "Thu");
        assert_eq!(strftime_utc("%A", 0), "Thursday");
        assert_eq!(strftime_utc("%b", 0), "Jan");
        assert_eq!(strftime_utc("%B", 0), "January");
        assert_eq!(strftime_utc("%h", 0), "Jan");
        assert_eq!(strftime_utc("%u", 0), "4");
        assert_eq!(strftime_utc("%w", 0), "4");
        assert_eq!(strftime_utc("%j", 0), "001");
        assert_eq!(strftime_utc("%U", 0), "00");
        assert_eq!(strftime_utc("%W", 0), "00");
        assert_eq!(strftime_utc("%V", 0), "01");
        assert_eq!(strftime_utc("%G", 0), "1970");
        assert_eq!(strftime_utc("%g", 0), "70");
        assert_eq!(strftime_utc("%c", 0), "Thu 01 Jan 1970 12:00:00 AM UTC");
        assert_eq!(strftime_utc("%x", 0), "01/01/1970");
        assert_eq!(strftime_utc("%X", 0), "12:00:00 AM");
    }

    #[test]
    fn strftime_pad_modifiers() {
        assert_eq!(strftime_utc("%-m/%-d/%Y", 0), "1/1/1970");
        assert_eq!(strftime_utc("%_m/%_d/%Y", 0), " 1/ 1/1970");
        assert_eq!(strftime_utc("%0m/%0d/%Y", 0), "01/01/1970");
    }

    #[test]
    fn strftime_edges_and_invalid_formats() {
        assert_eq!(strftime_utc("%", 0), "%");
        assert_eq!(strftime_utc("%%", 0), "%");
        assert_eq!(strftime_utc("%q", 0), "%q");
        assert_eq!(strftime_utc("", 0), "");
        assert_eq!(strftime_utc(&"a".repeat(255), 0), "a".repeat(255));
        assert_eq!(strftime_utc(&"a".repeat(256), 0), "");
    }

    #[test]
    fn strftime_builtin_call_contract() {
        let res = call_strftime_builtin(&[Typval::String(OxStr::from("%s"))]).unwrap();
        let Typval::String(s) = res else { panic!("expected string") };
        assert!(!s.0.is_empty());
        assert!(s.to_string_lossy().chars().all(|c| c.is_ascii_digit()));

        let res = call_strftime_builtin(&[
            Typval::String(OxStr::from("%Y")),
            Typval::Number(9_223_372_036_854_775_807),
        ])
        .unwrap();
        assert_eq!(res, Typval::String(OxStr::from("(Invalid)")));

        let res = call_strftime_builtin(&[
            Typval::String(OxStr::from("%Y")),
            Typval::Number(-9_223_372_036_854_775_808),
        ])
        .unwrap();
        assert_eq!(res, Typval::String(OxStr::from("(Invalid)")));

        assert_eq!(
            call_strftime_builtin(&[]).unwrap_err().code,
            "E119"
        );
        assert_eq!(
            call_strftime_builtin(&[
                Typval::String(OxStr::from("%Y")),
                Typval::Number(0),
                Typval::Number(0),
            ])
            .unwrap_err()
            .code,
            "E118"
        );
        assert_eq!(
            call_strftime_builtin(&[Typval::list(vec![])])
                .unwrap_err()
                .code,
            "E730"
        );
        assert_eq!(
            call_strftime_builtin(&[
                Typval::String(OxStr::from("%Y")),
                Typval::list(vec![]),
            ])
            .unwrap_err()
            .code,
            "E745"
        );
    }
}
