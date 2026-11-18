use ox_text::Position;

use crate::buffer::BufferTextEditRequest;
use crate::extmark::ExtmarkPosition;
use crate::motion;
use crate::register::{RegisterContent, RegisterError, RegisterKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PutDirection {
    Before,
    After,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PutEdit {
    Splice(BufferTextEditRequest),
    InsertLines {
        after_lnum: usize,
        lines: Vec<Vec<u8>>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PutPlan {
    pub(crate) edits: Vec<PutEdit>,
    pub(crate) cursor_before: Position,
    pub(crate) cursor_after: Position,
    /// Buffer `'[` mark: first inserted byte (`b_op_start`, `register.c`).
    pub(crate) mark_start: Position,
    /// Buffer `']` mark: last inserted byte (`b_op_end`, `register.c`).
    pub(crate) mark_end: Position,
}

/// Resolves the insertion origin for one register put.
///
/// `origin_line` is the content of target row `cursor.lnum`, which the caller
/// must have clamped into `1..=line_count` (empty when the buffer has no rows).
/// Linewise puts read neither argument; other kinds extend past the scalar at
/// `cursor.col` for [`PutDirection::After`], which is why they need the row.
#[must_use]
pub(crate) fn put_origin(
    origin_line: &[u8],
    line_count: usize,
    cursor: Position,
    kind: RegisterKind,
    direction: PutDirection,
) -> Position {
    if matches!(kind, RegisterKind::LineWise) {
        return Position {
            lnum: match direction {
                PutDirection::Before => cursor.lnum.saturating_sub(1),
                PutDirection::After => cursor.lnum,
            },
            col: 0,
        };
    }

    let lnum = cursor.lnum.clamp(1, line_count.max(1));
    let col = match direction {
        PutDirection::Before => cursor.col,
        PutDirection::After => {
            motion::next_char_boundary(origin_line, cursor.col).min(origin_line.len())
        }
    };
    Position { lnum, col }
}

/// Inputs for one register put, grouped so callers keep a stable order.
///
/// `target_lines[i]` is the content of target row `origin.lnum + i` and
/// `line_count` is the target row count. Characterwise plans read only the
/// first row (to cap a cursor-at-end column); linewise plans read neither;
/// only the blockwise plan reads past the origin row, one row per register
/// row, which is why the slice is threaded at all. `cursend` is upstream
/// `PUT_CURSEND` (`gp`/`gP`): the cursor lands after the inserted text
/// instead of on its first byte.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PutRequest<'a> {
    pub(crate) target_lines: &'a [Vec<u8>],
    pub(crate) line_count: usize,
    pub(crate) origin: Position,
    pub(crate) content: &'a RegisterContent,
    pub(crate) count: usize,
    pub(crate) cursor_before: Position,
    pub(crate) cursend: bool,
}

/// Plans the edits for one register put.
pub(crate) fn plan_put(request: &PutRequest<'_>) -> Result<PutPlan, RegisterError> {
    let count = request.count.max(1);
    match request.content.kind() {
        RegisterKind::CharacterWise => plan_characterwise(request, count),
        RegisterKind::LineWise => plan_linewise(request, count),
        RegisterKind::BlockWise { width } => plan_blockwise(request, width, count),
    }
}

fn plan_characterwise(request: &PutRequest<'_>, count: usize) -> Result<PutPlan, RegisterError> {
    let PutRequest {
        origin,
        content,
        cursor_before,
        cursend,
        target_lines,
        ..
    } = *request;
    if content.lines().len() == 1 {
        let line = &content.lines()[0];
        let byte_len = line
            .len()
            .checked_mul(count)
            .ok_or(RegisterError::PositionOverflow)?;
        if byte_len == 0 {
            return Ok(PutPlan {
                edits: Vec::new(),
                cursor_before,
                cursor_after: cursor_before,
                mark_start: cursor_before,
                mark_end: cursor_before,
            });
        }

        let mut payload = Vec::with_capacity(byte_len);
        for _ in 0..count {
            payload.extend_from_slice(line);
        }
        let last_start = byte_len - last_scalar_len(&payload);
        // `'[` is the put position and `']` the last inserted byte
        // (`b_op_start`/`b_op_end`, `register.c:do_put`).
        let mark_start = origin;
        let mark_end = Position {
            lnum: origin.lnum,
            col: origin
                .col
                .checked_add(last_start)
                .ok_or(RegisterError::PositionOverflow)?,
        };
        let cursor_after = if cursend {
            // `PUT_CURSEND`: past the last byte, capped at the new line end.
            let new_len = target_lines
                .first()
                .map_or(usize::MAX, Vec::len)
                .saturating_add(byte_len);
            Position {
                lnum: origin.lnum,
                col: origin
                    .col
                    .checked_add(byte_len)
                    .ok_or(RegisterError::PositionOverflow)?
                    .min(new_len),
            }
        } else {
            mark_end
        };
        return Ok(PutPlan {
            edits: vec![splice(origin, vec![payload])],
            cursor_before,
            cursor_after,
            mark_start,
            mark_end,
        });
    }

    let bytes = content.to_bytes();
    let byte_len = bytes
        .len()
        .checked_mul(count)
        .ok_or(RegisterError::PositionOverflow)?;
    let mut stream = Vec::with_capacity(byte_len);
    for _ in 0..count {
        stream.extend_from_slice(&bytes);
    }
    let replacement: Vec<Vec<u8>> = stream
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    let last_row = replacement.last().map_or(&[][..], Vec::as_slice);
    let last_start = last_row.len().saturating_sub(last_scalar_len(last_row));
    let mark_start = origin;
    let mark_end = Position {
        lnum: origin
            .lnum
            .checked_add(replacement.len().saturating_sub(1))
            .ok_or(RegisterError::PositionOverflow)?,
        col: if replacement.len() == 1 {
            origin
                .col
                .checked_add(last_start)
                .ok_or(RegisterError::PositionOverflow)?
        } else {
            last_start
        },
    };
    let cursor_after = if cursend {
        // Past the last inserted byte; the last row starts at column zero
        // when the put spans lines, capped at the new line end.
        let suffix_len = target_lines
            .first()
            .map_or(0, |row| row.len().saturating_sub(origin.col));
        let new_len = last_row.len().saturating_add(suffix_len);
        if replacement.len() == 1 {
            Position {
                lnum: origin.lnum,
                col: origin
                    .col
                    .checked_add(last_row.len())
                    .ok_or(RegisterError::PositionOverflow)?
                    .min(new_len),
            }
        } else {
            Position {
                lnum: mark_end.lnum,
                col: last_row.len().min(new_len),
            }
        }
    } else {
        origin
    };
    Ok(PutPlan {
        edits: vec![splice(origin, replacement)],
        cursor_before,
        cursor_after,
        mark_start,
        mark_end,
    })
}

fn plan_linewise(request: &PutRequest<'_>, count: usize) -> Result<PutPlan, RegisterError> {
    let PutRequest {
        origin,
        content,
        cursor_before,
        cursend,
        line_count,
        ..
    } = *request;
    let row_count = content
        .lines()
        .len()
        .checked_mul(count)
        .ok_or(RegisterError::PositionOverflow)?;
    let mut repeated = Vec::with_capacity(row_count);
    for _ in 0..count {
        repeated.extend(content.lines().iter().cloned());
    }
    let first = origin
        .lnum
        .checked_add(1)
        .ok_or(RegisterError::PositionOverflow)?;
    let last = origin
        .lnum
        .checked_add(row_count)
        .ok_or(RegisterError::PositionOverflow)?;
    let mark_start = Position { lnum: first, col: 0 };
    // `']` is the last inserted byte (`register.c`: `col - 1` adjusted for
    // the final character width, zero for an empty last line).
    let last_row = repeated.last().map_or(&[][..], Vec::as_slice);
    let mark_end = Position {
        lnum: last,
        col: if last_row.is_empty() {
            0
        } else {
            last_row.len().saturating_sub(last_scalar_len(last_row))
        },
    };
    let cursor_after = if cursend {
        // `PUT_CURSEND` on lines: the line after the inserted text.
        Position {
            lnum: last
                .checked_add(1)
                .ok_or(RegisterError::PositionOverflow)?
                .min(line_count.saturating_add(row_count)),
            col: 0,
        }
    } else {
        Position {
            lnum: first,
            col: first_nonblank(&repeated[0]),
        }
    };
    Ok(PutPlan {
        edits: vec![PutEdit::InsertLines {
            after_lnum: origin.lnum,
            lines: repeated,
        }],
        cursor_before,
        cursor_after,
        mark_start,
        mark_end,
    })
}

fn plan_blockwise(request: &PutRequest<'_>, width: usize, count: usize) -> Result<PutPlan, RegisterError> {
    let PutRequest {
        target_lines,
        line_count,
        origin,
        content,
        cursor_before,
        cursend,
        ..
    } = *request;
    let first_target = origin
        .lnum
        .checked_sub(1)
        .ok_or(RegisterError::PositionOverflow)?;
    let mut edits = Vec::with_capacity(content.lines().len().saturating_add(1));
    let mut tail = Vec::new();
    // Last inserted byte for `']` (`register.c`: `bd.textcol + totlen - 1`).
    let mut mark_end = origin;

    for (row_index, row) in content.lines().iter().enumerate() {
        let target = first_target
            .checked_add(row_index)
            .ok_or(RegisterError::PositionOverflow)?;
        // `y_width` and the padding are display cells, not bytes: a
        // multibyte row pads to its cell width (`update_yankreg_width`).
        let padding = width.saturating_sub(RegisterContent::display_cells(row));
        let unit_len = row
            .len()
            .checked_add(padding)
            .ok_or(RegisterError::PositionOverflow)?;

        if let Some(target_line) = target_lines.get(row_index) {
            let shortline = origin.col >= target_line.len();
            let inserted = if shortline {
                build_short_row(origin.col - target_line.len(), row, padding, count)?
            } else {
                repeat_unit(row, padding, count, unit_len)?
            };
            if !inserted.is_empty() {
                let col = if shortline {
                    target_line.len()
                } else {
                    origin.col
                };
                mark_end = Position {
                    lnum: target
                        .checked_add(1)
                        .ok_or(RegisterError::PositionOverflow)?,
                    col: col
                        .checked_add(inserted.len().saturating_sub(last_scalar_len(&inserted)))
                        .ok_or(RegisterError::PositionOverflow)?,
                };
                edits.push(PutEdit::Splice(BufferTextEditRequest {
                    start: ExtmarkPosition::new(target, col),
                    end: ExtmarkPosition::new(target, col),
                    replacement: vec![inserted],
                }));
            }
        } else {
            let inserted = build_short_row(origin.col, row, padding, count)?;
            mark_end = Position {
                lnum: line_count
                    .checked_add(tail.len())
                    .and_then(|lnum| lnum.checked_add(1))
                    .ok_or(RegisterError::PositionOverflow)?,
                col: inserted
                    .len()
                    .saturating_sub(last_scalar_len(&inserted)),
            };
            tail.push(inserted);
        }
    }

    if !tail.is_empty() {
        edits.push(PutEdit::InsertLines {
            after_lnum: line_count,
            lines: tail,
        });
    }
    let cursor_after = if cursend {
        Position {
            lnum: mark_end.lnum,
            col: mark_end.col.saturating_add(1),
        }
    } else {
        origin
    };
    Ok(PutPlan {
        edits,
        cursor_before,
        cursor_after,
        mark_start: origin,
        mark_end,
    })
}

fn build_short_row(
    leading_spaces: usize,
    row: &[u8],
    padding: usize,
    count: usize,
) -> Result<Vec<u8>, RegisterError> {
    let unit_len = row
        .len()
        .checked_add(padding)
        .ok_or(RegisterError::PositionOverflow)?;
    let repeated_len = unit_len
        .checked_mul(count - 1)
        .ok_or(RegisterError::PositionOverflow)?;
    let len = leading_spaces
        .checked_add(repeated_len)
        .and_then(|len| len.checked_add(row.len()))
        .ok_or(RegisterError::PositionOverflow)?;
    let mut inserted = Vec::with_capacity(len);
    inserted.resize(leading_spaces, b' ');
    for _ in 1..count {
        inserted.extend_from_slice(row);
        inserted.resize(inserted.len() + padding, b' ');
    }
    inserted.extend_from_slice(row);
    Ok(inserted)
}

fn repeat_unit(
    row: &[u8],
    padding: usize,
    count: usize,
    unit_len: usize,
) -> Result<Vec<u8>, RegisterError> {
    let len = unit_len
        .checked_mul(count)
        .ok_or(RegisterError::PositionOverflow)?;
    let mut inserted = Vec::with_capacity(len);
    for _ in 0..count {
        inserted.extend_from_slice(row);
        inserted.resize(inserted.len() + padding, b' ');
    }
    Ok(inserted)
}

fn splice(origin: Position, replacement: Vec<Vec<u8>>) -> PutEdit {
    let position = ExtmarkPosition::new(origin.lnum - 1, origin.col);
    PutEdit::Splice(BufferTextEditRequest {
        start: position,
        end: position,
        replacement,
    })
}

pub(crate) fn first_nonblank(line: &[u8]) -> usize {
    line.iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(0)
}

pub(crate) fn last_scalar_len(bytes: &[u8]) -> usize {
    let mut len = usize::from(!bytes.is_empty());
    while len < bytes.len() && bytes[bytes.len() - len] & 0b1100_0000 == 0b1000_0000 {
        len += 1;
    }
    len.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(lnum: usize, col: usize) -> Position {
        Position { lnum, col }
    }

    fn request<'a>(
        target_lines: &'a [Vec<u8>],
        line_count: usize,
        origin: Position,
        content: &'a RegisterContent,
        count: usize,
        cursor_before: Position,
    ) -> PutRequest<'a> {
        PutRequest {
            target_lines,
            line_count,
            origin,
            content,
            count,
            cursor_before,
            cursend: false,
        }
    }

    fn splice_replacement(plan: &PutPlan) -> &[Vec<u8>] {
        let PutEdit::Splice(request) = &plan.edits[0] else {
            panic!("expected splice");
        };
        &request.replacement
    }

    #[test]
    fn one_line_count_expands_and_cursor_uses_last_scalar_start() {
        let content = RegisterContent::characterwise("한X".as_bytes()).unwrap();
        let plan = plan_put(&request(&[], 0, position(1, 1), &content, 2, position(1, 0))).unwrap();

        assert_eq!(splice_replacement(&plan), &["한X한X".as_bytes().to_vec()]);
        assert_eq!(plan.cursor_after, position(1, 8));
        assert_eq!(plan.mark_start, position(1, 1));
        assert_eq!(plan.mark_end, position(1, 8));
        assert_eq!(last_scalar_len("한".as_bytes()), 3);
        assert_eq!(last_scalar_len(b"X"), 1);
    }

    #[test]
    fn one_line_cursor_at_end_stops_after_inserted_text() {
        let content = RegisterContent::characterwise("한X".as_bytes()).unwrap();
        let origin_row = vec!["z".as_bytes().to_vec()];
        let plan = plan_put(&PutRequest {
            target_lines: &origin_row,
            line_count: 1,
            origin: position(1, 1),
            content: &content,
            count: 2,
            cursor_before: position(1, 0),
            cursend: true,
        })
        .unwrap();

        // Eight inserted bytes after a one-byte row: `1 + 8`, capped there.
        assert_eq!(plan.cursor_after, position(1, 9));
        assert_eq!(plan.mark_end, position(1, 8));
    }

    #[test]
    fn multiline_count_joins_copy_boundaries() {
        let content = RegisterContent::characterwise(b"x\ny").unwrap();
        let plan = plan_put(&request(&[], 0, position(1, 1), &content, 2, position(1, 0))).unwrap();

        assert_eq!(
            splice_replacement(&plan),
            &[b"x".to_vec(), b"yx".to_vec(), b"y".to_vec()]
        );
        assert_eq!(plan.cursor_after, position(1, 1));
        assert_eq!(plan.mark_start, position(1, 1));
        assert_eq!(plan.mark_end, position(3, 0));
    }

    #[test]
    fn linewise_count_repeats_vertically_and_finds_first_nonblank() {
        let content = RegisterContent::linewise(vec![b"  x".to_vec(), b"y".to_vec()]).unwrap();
        let plan = plan_put(&request(&[], 0, position(0, 0), &content, 2, position(1, 0))).unwrap();

        assert_eq!(
            plan.edits,
            vec![PutEdit::InsertLines {
                after_lnum: 0,
                lines: vec![
                    b"  x".to_vec(),
                    b"y".to_vec(),
                    b"  x".to_vec(),
                    b"y".to_vec()
                ],
            }]
        );
        assert_eq!(plan.cursor_after, position(1, 2));
        assert_eq!(plan.mark_start, position(1, 0));
        assert_eq!(plan.mark_end, position(4, 0));
        assert_eq!(first_nonblank(b" \t "), 0);
    }

    #[test]
    fn linewise_cursor_at_end_selects_following_line() {
        let content = RegisterContent::linewise(vec![b"ab".to_vec()]).unwrap();
        let plan = plan_put(&PutRequest {
            target_lines: &[],
            line_count: 3,
            origin: position(1, 0),
            content: &content,
            count: 1,
            cursor_before: position(1, 0),
            cursend: true,
        })
        .unwrap();

        assert_eq!(plan.cursor_after, position(3, 0));
        assert_eq!(plan.mark_end, position(2, 1));
    }

    #[test]
    fn blockwise_distinguishes_short_rows_from_following_text() {
        let content = RegisterContent::blockwise(vec![b"Q".to_vec(), b"R".to_vec()], 2).unwrap();
        let target = [b"abcdef".to_vec(), b"a".to_vec()];
        let plan = plan_put(&request(
            &target,
            2,
            position(1, 3),
            &content,
            2,
            position(1, 2),
        ))
        .unwrap();

        assert_eq!(splice_replacement(&plan), &[b"Q Q ".to_vec()]);
        let PutEdit::Splice(short) = &plan.edits[1] else {
            panic!("expected short-row splice");
        };
        assert_eq!(short.start, ExtmarkPosition::new(1, 1));
        assert_eq!(short.replacement, vec![b"  R R".to_vec()]);
        assert_eq!(plan.mark_start, position(1, 3));
    }

    #[test]
    fn blockwise_pads_multibyte_rows_by_display_cells() {
        // `À` is two bytes but one cell; the width-12 rectangle pads eleven
        // cells, not ten (`update_yankreg_width`, `register.c`).
        let content = RegisterContent::blockwise(
            vec!["À".as_bytes().to_vec(), "ÀÀ".as_bytes().to_vec()],
            12,
        )
        .unwrap();
        let target = [b" 1".to_vec(), b" 2".to_vec()];
        let plan = plan_put(&request(
            &target,
            2,
            position(1, 1),
            &content,
            1,
            position(1, 0),
        ))
        .unwrap();

        assert_eq!(
            splice_replacement(&plan),
            &[[
                b"\xc3\x80".as_slice(),
                &[b' '; 11],
            ]
            .concat()]
        );
        let PutEdit::Splice(second) = &plan.edits[1] else {
            panic!("expected second-row splice");
        };
        assert_eq!(
            second.replacement,
            vec![[b"\xc3\x80\xc3\x80".as_slice(), &[b' '; 10]].concat()]
        );
    }

    #[test]
    fn blockwise_materializes_uniform_eof_tail_rows() {
        let content =
            RegisterContent::blockwise(vec![b"Q".to_vec(), b"R".to_vec(), b"S".to_vec()], 1)
                .unwrap();
        let target = [b"abc".to_vec()];
        let plan = plan_put(&request(
            &target,
            1,
            position(1, 2),
            &content,
            1,
            position(1, 1),
        ))
        .unwrap();

        assert_eq!(splice_replacement(&plan), &[b"Q".to_vec()]);
        assert_eq!(
            plan.edits[1],
            PutEdit::InsertLines {
                after_lnum: 1,
                lines: vec![b"  R".to_vec(), b"  S".to_vec()],
            }
        );
    }

    #[test]
    fn count_overflow_is_reported_before_expansion() {
        let one_line = RegisterContent::characterwise(b"xx").unwrap();
        assert_eq!(
            plan_put(&request(&[], 0, position(1, 0), &one_line, usize::MAX, position(1, 0))),
            Err(RegisterError::PositionOverflow)
        );

        let linewise = RegisterContent::linewise(vec![b"x".to_vec(), b"y".to_vec()]).unwrap();
        assert_eq!(
            plan_put(&request(
                &[],
                0,
                position(0, 0),
                &linewise,
                usize::MAX,
                position(1, 0)
            )),
            Err(RegisterError::PositionOverflow)
        );
    }

    #[test]
    fn empty_characterwise_content_produces_empty_plan() {
        let content = RegisterContent::characterwise(b"").unwrap();
        let before = position(1, 0);
        let plan = plan_put(&request(&[], 0, before, &content, 3, before)).unwrap();

        assert!(plan.edits.is_empty());
        assert_eq!(plan.cursor_after, before);
    }
}
