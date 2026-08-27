//! The token under the cursor.
//!
//! Splitting the line is deliberately no backend's job: doing it once here is
//! what lets a trigger character select a backend, and keeps the byte-offset
//! handling in a single place rather than repeated per trigger.

use std::ops::Range;

/// A whitespace-delimited token, split at its first character.
pub struct Token {
    /// First character: what selects the backend.
    pub trigger: char,
    /// Everything after the trigger. The trigger always survives the
    /// replacement, so no backend does offset arithmetic.
    pub range: Range<usize>,
}

/// The token under `cursor`, or `None` when the cursor is not inside one.
/// Sitting directly before a token does not count: `abc |@src` is not a
/// mention yet.
pub fn at(line: &str, cursor: usize) -> Option<Token> {
    let cursor = floor_char_boundary(line, cursor);
    let start = line[..cursor]
        .char_indices()
        .rev()
        .take_while(|&(_, character)| !character.is_whitespace())
        .map(|(index, _)| index)
        .last()?;
    let end = line[cursor..]
        .find(char::is_whitespace)
        .map_or(line.len(), |index| cursor + index);
    // `start < end`, so the token has at least its trigger character.
    let trigger = line[start..end].chars().next()?;
    Some(Token {
        trigger,
        range: start + trigger.len_utf8()..end,
    })
}

/// The cursor arrives as a raw byte offset, and slicing a `str` off a char
/// boundary panics.
fn floor_char_boundary(line: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(line.len());
    // Terminates: byte 0 is always a boundary.
    while !line.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_split_at_its_trigger() {
        let token = at("explain @mai", 12).unwrap();

        assert_eq!(token.trigger, '@');
        // The `@` at byte 8 is outside the range, so it survives replacement.
        assert_eq!(token.range, 9..12);
    }

    /// The cursor sitting inside the token still yields the whole of it.
    #[test]
    fn the_whole_token_is_taken_from_inside_it() {
        assert_eq!(at("@foo", 3).unwrap().range, 1..4);
    }

    /// A bare trigger is an empty pattern, not the absence of a token.
    #[test]
    fn a_lone_trigger_is_still_a_token() {
        let token = at("@", 1).unwrap();

        assert_eq!(token.trigger, '@');
        assert_eq!(token.range, 1..1);
    }

    #[test]
    fn there_is_no_token_in_empty_space_or_before_one() {
        assert!(at("", 0).is_none());
        // The cursor is before the `@`, so it is not inside the token yet.
        assert!(at("abc @src", 4).is_none());
    }

    /// The cursor arrives as a raw byte offset and the line is sliced by byte,
    /// so an offset inside a character must not panic.
    #[test]
    fn a_cursor_off_a_char_boundary_does_not_panic() {
        // `@é` is 3 bytes: byte 2 lands inside the `é`, byte 99 past the end.
        for cursor in [2, 99] {
            assert_eq!(at("@é", cursor).unwrap().trigger, '@');
        }
    }

    /// A multi-byte trigger is measured in bytes, not characters.
    #[test]
    fn a_multibyte_trigger_is_excluded_by_its_own_width() {
        let token = at("émai", 4).unwrap();

        assert_eq!(token.trigger, 'é');
        assert_eq!(token.range, 2..5);
    }
}
