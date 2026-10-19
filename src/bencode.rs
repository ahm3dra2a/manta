use anyhow::{Context, Result, bail};

#[derive(Debug, PartialEq, Eq)]
pub enum Value {
    Integer(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dictionary(Vec<(Vec<u8>, Value)>),
}

impl Value {
    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        match self {
            Self::Dictionary(entries) => entries
                .iter()
                .find_map(|(candidate, value)| (candidate == key).then_some(value)),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Result<&[u8]> {
        match self {
            Self::Bytes(bytes) => Ok(bytes),
            _ => bail!("expected a byte string"),
        }
    }

    pub fn as_integer(&self) -> Result<i64> {
        match self {
            Self::Integer(value) => Ok(*value),
            _ => bail!("expected an integer"),
        }
    }

    pub fn as_list(&self) -> Result<&[Value]> {
        match self {
            Self::List(values) => Ok(values),
            _ => bail!("expected a list"),
        }
    }
}

#[derive(Debug)]
pub struct ParsedValue {
    pub value: Value,
    pub end: usize,
}

pub fn parse(input: &[u8]) -> Result<ParsedValue> {
    let parsed = parse_prefix(input)?;
    if parsed.end != input.len() {
        bail!("unexpected data after bencoded value");
    }
    Ok(parsed)
}

pub fn parse_prefix(input: &[u8]) -> Result<ParsedValue> {
    let mut parser = Parser { input, position: 0 };
    parser.parse_value(0)
}

pub fn parse_with_trailing_whitespace(input: &[u8]) -> Result<ParsedValue> {
    let end = input
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |position| position + 1);
    parse(&input[..end])
}

struct Parser<'a> {
    input: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    fn parse_value(&mut self, depth: usize) -> Result<ParsedValue> {
        if depth > 128 {
            bail!("bencoded value exceeds maximum nesting depth");
        }
        let start = self.position;
        let marker = *self
            .input
            .get(self.position)
            .context("unexpected end of bencoded data")?;
        self.position += 1;

        let value = match marker {
            b'i' => Value::Integer(self.parse_integer()?),
            b'l' => {
                let mut values = Vec::new();
                while self.peek()? != b'e' {
                    values.push(self.parse_value(depth + 1)?.value);
                }
                self.position += 1;
                Value::List(values)
            }
            b'd' => {
                let mut entries = Vec::new();
                while self.peek()? != b'e' {
                    let key = self.parse_bytes()?;
                    let value = self.parse_value(depth + 1)?.value;
                    entries.push((key, value));
                }
                self.position += 1;
                Value::Dictionary(entries)
            }
            b'0'..=b'9' => Value::Bytes(self.parse_bytes_after_first(marker)?),
            _ => bail!("invalid bencode marker at byte {}", start),
        };

        Ok(ParsedValue {
            value,
            end: self.position,
        })
    }

    fn parse_integer(&mut self) -> Result<i64> {
        let end = self
            .input
            .get(self.position..)
            .context("unexpected end while reading integer")?
            .iter()
            .position(|byte| *byte == b'e')
            .map(|offset| self.position + offset)
            .context("unterminated bencoded integer")?;
        let digits = &self.input[self.position..end];
        if digits.is_empty()
            || digits == b"-0"
            || (digits[0] == b'0' && digits.len() > 1)
            || (digits[0] == b'-'
                && (digits.len() == 1
                    || digits[1] == b'0'
                    || !digits[1..].iter().all(u8::is_ascii_digit)))
            || (digits[0] != b'-' && !digits.iter().all(u8::is_ascii_digit))
        {
            bail!("invalid bencoded integer");
        }
        let value = std::str::from_utf8(digits)
            .context("bencoded integer is not ASCII")?
            .parse()
            .context("bencoded integer is out of range")?;
        self.position = end + 1;
        Ok(value)
    }

    fn parse_bytes(&mut self) -> Result<Vec<u8>> {
        let first = *self
            .input
            .get(self.position)
            .context("unexpected end while reading byte-string length")?;
        if !first.is_ascii_digit() {
            bail!("dictionary key is not a bencoded byte string");
        }
        self.position += 1;
        self.parse_bytes_after_first(first)
    }

    fn parse_bytes_after_first(&mut self, first: u8) -> Result<Vec<u8>> {
        let length = self.parse_length(first)?;
        let end = self
            .position
            .checked_add(length)
            .context("byte-string length overflow")?;
        let bytes = self
            .input
            .get(self.position..end)
            .context("byte string extends past end of input")?
            .to_vec();
        self.position = end;
        Ok(bytes)
    }

    fn parse_length(&mut self, first: u8) -> Result<usize> {
        let mut digits = vec![first];
        while self.peek()? != b':' {
            let byte = self.peek()?;
            if !byte.is_ascii_digit() {
                bail!("invalid byte-string length");
            }
            digits.push(byte);
            self.position += 1;
        }
        self.position += 1;
        if digits.len() > 1 && digits[0] == b'0' {
            bail!("byte-string length has a leading zero");
        }
        std::str::from_utf8(&digits)
            .context("byte-string length is not ASCII")?
            .parse()
            .context("byte-string length is out of range")
    }

    fn peek(&self) -> Result<u8> {
        self.input
            .get(self.position)
            .copied()
            .context("unexpected end of bencoded data")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_values_and_preserves_dictionary_entries() {
        let parsed = parse(b"d4:infod1:ai1ee4:name4:teste").unwrap();
        assert_eq!(parsed.end, 28);
        assert_eq!(
            parsed.value.get(b"name").unwrap().as_bytes().unwrap(),
            b"test"
        );
    }

    #[test]
    fn rejects_malformed_and_trailing_data() {
        for invalid in [&b"i-0e"[..], &b"03:abc"[..], &b"i1ex"[..]] {
            assert!(parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn accepts_only_whitespace_after_a_tracker_value() {
        assert!(parse_with_trailing_whitespace(b"d1:ai1ee\r\n").is_ok());
        assert!(parse_with_trailing_whitespace(b"d1:ai1ee<html>").is_err());
    }
}
