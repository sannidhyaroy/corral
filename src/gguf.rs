//! Reading one value from a GGUF model file's header without loading the model: the header is a table of typed
//! key-value pairs (the chat template among them), walked entry by entry. Large entries such as the vocabulary are
//! skipped, never held in memory.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek};
use std::path::Path;

const MAGIC: &[u8; 4] = b"GGUF";
/// A string longer than this is a corrupt or hostile file, not a header value.
const LONGEST_STRING: u64 = 64 << 20;

/// The model's chat template (`tokenizer.chat_template`), if the file has one.
pub fn chat_template(path: &Path) -> io::Result<Option<String>> {
    string_value(&mut BufReader::new(File::open(path)?), "tokenizer.chat_template")
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_string(reader: &mut impl Read) -> io::Result<String> {
    let length = read_u64(reader)?;
    if length > LONGEST_STRING {
        return Err(invalid("a header string is implausibly long"));
    }
    let mut bytes = vec![0u8; usize::try_from(length).map_err(|_| invalid("string too long"))?];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|_| invalid("a header string isn't UTF-8"))
}

fn skip_string<R: Read + Seek>(reader: &mut BufReader<R>) -> io::Result<()> {
    let length = read_u64(reader)?;
    let offset = i64::try_from(length).map_err(|_| invalid("string too long"))?;
    reader.seek_relative(offset)
}

/// Size in bytes of a fixed-size value type, `None` for strings and arrays.
fn fixed_size(value_type: u32) -> Option<i64> {
    match value_type {
        0 | 1 | 7 => Some(1), // u8, i8, bool
        2 | 3 => Some(2),     // u16, i16
        4..=6 => Some(4),     // u32, i32, f32
        10..=12 => Some(8),   // u64, i64, f64
        _ => None,
    }
}

/// Skips a value. `seek_relative` keeps the read buffer when the target is inside it, which matters for the
/// vocabulary: hundreds of thousands of short strings in a row.
fn skip_value<R: Read + Seek>(reader: &mut BufReader<R>, value_type: u32) -> io::Result<()> {
    if let Some(size) = fixed_size(value_type) {
        return reader.seek_relative(size);
    }
    match value_type {
        8 => skip_string(reader),
        9 => {
            let element_type = read_u32(reader)?;
            let count = read_u64(reader)?;
            if let Some(size) = fixed_size(element_type) {
                let bytes = i64::try_from(count)
                    .ok()
                    .and_then(|count| count.checked_mul(size))
                    .ok_or_else(|| invalid("array too long"))?;
                return reader.seek_relative(bytes);
            }
            for _ in 0..count {
                skip_value(reader, element_type)?;
            }
            Ok(())
        }
        _ => Err(invalid("unknown value type in the header")),
    }
}

/// The string value of `wanted` in a GGUF header, or `None` if the header doesn't have it.
fn string_value<R: Read + Seek>(reader: &mut BufReader<R>, wanted: &str) -> io::Result<Option<String>> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid("not a GGUF file"));
    }
    let version = read_u32(reader)?;
    if version < 2 {
        return Err(invalid("GGUF version 1 isn't supported"));
    }
    let _tensor_count = read_u64(reader)?;
    let entries = read_u64(reader)?;
    for _ in 0..entries {
        let key = read_string(reader)?;
        let value_type = read_u32(reader)?;
        if key == wanted {
            return if value_type == 8 {
                read_string(reader).map(Some)
            } else {
                Err(invalid("the value isn't a string"))
            };
        }
        skip_value(reader, value_type)?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn string(text: &str) -> Vec<u8> {
        let mut bytes = (text.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    /// A small GGUF header: a number, an array of strings (like a vocabulary), an array of floats, then the template.
    fn header() -> Vec<u8> {
        let mut file = b"GGUF".to_vec();
        file.extend(3u32.to_le_bytes());
        file.extend(0u64.to_le_bytes());
        file.extend(4u64.to_le_bytes());
        file.extend(string("general.alignment"));
        file.extend(4u32.to_le_bytes());
        file.extend(32u32.to_le_bytes());
        file.extend(string("tokenizer.ggml.tokens"));
        file.extend(9u32.to_le_bytes());
        file.extend(8u32.to_le_bytes());
        file.extend(3u64.to_le_bytes());
        for token in ["<s>", "hello", "world"] {
            file.extend(string(token));
        }
        file.extend(string("tokenizer.ggml.scores"));
        file.extend(9u32.to_le_bytes());
        file.extend(6u32.to_le_bytes());
        file.extend(2u64.to_le_bytes());
        file.extend(1.5f32.to_le_bytes());
        file.extend(2.5f32.to_le_bytes());
        file.extend(string("tokenizer.chat_template"));
        file.extend(8u32.to_le_bytes());
        file.extend(string("{% if enable_thinking %}<think>{% endif %}"));
        file
    }

    #[test]
    fn finds_the_template_past_arrays() {
        let template = string_value(&mut BufReader::new(Cursor::new(header())), "tokenizer.chat_template").unwrap();
        assert_eq!(template.as_deref(), Some("{% if enable_thinking %}<think>{% endif %}"));
    }

    #[test]
    fn missing_keys_and_other_files() {
        assert_eq!(
            string_value(&mut BufReader::new(Cursor::new(header())), "general.name").unwrap(),
            None
        );
        let other = Cursor::new(b"PK\x03\x04 not a model".to_vec());
        assert!(string_value(&mut BufReader::new(other), "x").is_err());
    }
}
