//! Constant-memory scan of the top-level members of one JSON-RPC envelope.
//!
//! Both the stdio transport (a line the MCP codec rejected or the size limit
//! refused) and the LSP transport (a frame skipped for size) must tell what a
//! message was, and whom it addresses, without building it. [`EnvelopeScan`]
//! reads the bytes once, in any chunking, and keeps only whether `method`,
//! `result` or `error` appeared and a bounded copy of the `id` token.

use serde::de::DeserializeOwned;

const MAX_ID_BYTES: usize = 128;
const KEY_BYTES: usize = 6;
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Which top-level object member the scanner is positioned on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Id,
    Method,
    Result,
    Error,
    Other,
}

/// A top-level key, kept only as far as it can still equal a member of interest.
#[derive(Debug, Default)]
struct KeyBuf {
    bytes: [u8; KEY_BYTES],
    len: usize,
    matchable: bool,
}

impl KeyBuf {
    fn new() -> Self {
        Self {
            matchable: true,
            ..Self::default()
        }
    }

    fn push(&mut self, byte: u8) {
        match self.bytes.get_mut(self.len) {
            Some(cell) => {
                *cell = byte;
                self.len = self.len.saturating_add(1);
            }
            None => self.matchable = false,
        }
    }

    fn slot(&self) -> Slot {
        let key = self.bytes.get(..self.len).unwrap_or_default();
        match (self.matchable, key) {
            (true, b"id") => Slot::Id,
            (true, b"method") => Slot::Method,
            (true, b"result") => Slot::Result,
            (true, b"error") => Slot::Error,
            _ => Slot::Other,
        }
    }
}

/// The raw token of the top-level `id` member.
#[derive(Debug)]
enum IdCapture {
    Absent,
    /// The value has begun but its end was not seen, so it may be cut short.
    Partial(Vec<u8>),
    Complete(Vec<u8>),
    Unusable,
}

impl IdCapture {
    fn begin(&mut self) {
        *self = match self {
            Self::Absent => Self::Partial(Vec::new()),
            Self::Partial(_) | Self::Complete(_) | Self::Unusable => Self::Unusable,
        };
    }

    fn push(&mut self, byte: u8) {
        if let Self::Partial(raw) = self {
            if raw.len() < MAX_ID_BYTES {
                raw.push(byte);
            } else {
                *self = Self::Unusable;
            }
        }
    }

    fn complete(&mut self) {
        if let Self::Partial(raw) = self {
            *self = Self::Complete(std::mem::take(raw));
        }
    }

    fn mark_unusable(&mut self) {
        *self = Self::Unusable;
    }
}

#[derive(Debug)]
enum Phase {
    BeforeRoot,
    ExpectKey,
    InKey(KeyBuf),
    AfterKey(Slot),
    ValueStart(Slot),
    InValue(Slot),
    AfterValue,
    Finished,
    Malformed,
}

/// How far the scanned bytes got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// The top-level object was closed.
    Closed,
    /// The input ended inside a valid prefix of an object.
    Truncated,
    /// The input broke the structure of an object.
    Malformed,
}

/// What the scan saw of the top-level `id` member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdToken {
    /// No `id` member at the top level.
    Absent,
    /// The whole value token, at most 128 bytes, ended before the input did.
    Complete(Vec<u8>),
    /// Present but not capturable: nested, repeated, too long or cut short.
    Unusable,
}

impl IdToken {
    /// Whether an `id` member exists, whatever its value.
    pub(crate) const fn is_present(&self) -> bool {
        !matches!(self, Self::Absent)
    }

    /// The id as `T`, `None` when absent, unusable or not a `T`.
    pub(crate) fn decode<T: DeserializeOwned>(&self) -> Option<T> {
        match self {
            Self::Complete(raw) => serde_json::from_slice(raw).ok(),
            Self::Absent | Self::Unusable => None,
        }
    }
}

/// The result of a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// How far the scanned bytes got.
    pub(crate) shape: Shape,
    /// A `method` member began.
    pub(crate) method: bool,
    /// A `result` or `error` member began.
    pub(crate) outcome: bool,
    /// What the scan saw of the `id` member.
    pub(crate) id: IdToken,
}

/// Streaming scanner for the top-level `id`, `method`, `result` and `error`
/// members of one JSON object, in constant memory whatever the object's size.
///
/// It never panics on malformed input and never keeps more than 128 bytes of
/// the id. The id is captured only at depth one, so a nested `id` inside
/// `params` is never mistaken for the envelope's. A leading byte order mark is
/// skipped and member order does not matter. A key written with escapes is
/// never matched. Bytes after the closing brace are ignored.
#[derive(Debug)]
pub struct EnvelopeScan {
    phase: Phase,
    nested: usize,
    in_string: bool,
    escaped: bool,
    id: IdCapture,
    seen: Seen,
}

/// Which of `method`, `result` and `error` have begun.
#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    method: bool,
    outcome: bool,
}

impl Default for EnvelopeScan {
    fn default() -> Self {
        Self {
            phase: Phase::BeforeRoot,
            nested: 0,
            in_string: false,
            escaped: false,
            id: IdCapture::Absent,
            seen: Seen::default(),
        }
    }
}

const fn is_json_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

impl EnvelopeScan {
    /// Reads the next chunk of the input.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if matches!(self.phase, Phase::Finished | Phase::Malformed) {
                return;
            }
            self.step(byte);
        }
    }

    /// What the bytes fed so far amount to.
    pub(crate) fn finish(&self) -> Envelope {
        let shape = match self.phase {
            Phase::Finished => Shape::Closed,
            Phase::Malformed => Shape::Malformed,
            _ => Shape::Truncated,
        };
        let id = match &self.id {
            IdCapture::Absent => IdToken::Absent,
            IdCapture::Complete(raw) => IdToken::Complete(raw.clone()),
            IdCapture::Partial(_) | IdCapture::Unusable => IdToken::Unusable,
        };
        Envelope {
            shape,
            method: self.seen.method,
            outcome: self.seen.outcome,
            id,
        }
    }

    fn step(&mut self, byte: u8) {
        match &mut self.phase {
            Phase::BeforeRoot => match byte {
                b'{' => self.phase = Phase::ExpectKey,
                _ if is_json_space(byte) || BOM.contains(&byte) => {}
                _ => self.phase = Phase::Malformed,
            },
            Phase::ExpectKey => match byte {
                b'"' => {
                    self.escaped = false;
                    self.phase = Phase::InKey(KeyBuf::new());
                }
                b'}' => self.phase = Phase::Finished,
                _ if is_json_space(byte) => {}
                _ => self.phase = Phase::Malformed,
            },
            Phase::InKey(key) => {
                if self.escaped {
                    self.escaped = false;
                    key.matchable = false;
                } else {
                    match byte {
                        b'\\' => self.escaped = true,
                        b'"' => {
                            let slot = key.slot();
                            self.phase = Phase::AfterKey(slot);
                        }
                        _ => key.push(byte),
                    }
                }
            }
            Phase::AfterKey(slot) => {
                let slot = *slot;
                match byte {
                    b':' => self.phase = Phase::ValueStart(slot),
                    _ if is_json_space(byte) => {}
                    _ => self.phase = Phase::Malformed,
                }
            }
            Phase::ValueStart(slot) => {
                if !is_json_space(byte) {
                    let slot = *slot;
                    self.begin_value(slot);
                    self.phase = Phase::InValue(slot);
                    self.value_byte(slot, byte);
                }
            }
            Phase::InValue(slot) => {
                let slot = *slot;
                self.value_byte(slot, byte);
            }
            Phase::AfterValue => match byte {
                b',' => self.phase = Phase::ExpectKey,
                b'}' => self.phase = Phase::Finished,
                _ if is_json_space(byte) => {}
                _ => self.phase = Phase::Malformed,
            },
            Phase::Finished | Phase::Malformed => {}
        }
    }

    fn begin_value(&mut self, slot: Slot) {
        self.nested = 0;
        self.in_string = false;
        self.escaped = false;
        match slot {
            Slot::Id => self.id.begin(),
            Slot::Method => self.seen.method = true,
            Slot::Result | Slot::Error => self.seen.outcome = true,
            Slot::Other => {}
        }
    }

    fn end_value(&mut self, slot: Slot) {
        if slot == Slot::Id {
            self.id.complete();
        }
    }

    fn value_byte(&mut self, slot: Slot, byte: u8) {
        let capture = slot == Slot::Id;
        if self.in_string {
            if capture {
                self.id.push(byte);
            }
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
                if self.nested == 0 {
                    self.end_value(slot);
                    self.phase = Phase::AfterValue;
                }
            }
            return;
        }
        match byte {
            b'"' => {
                self.in_string = true;
                if capture {
                    self.id.push(byte);
                }
            }
            b'{' | b'[' => {
                self.nested = self.nested.saturating_add(1);
                if capture {
                    self.id.mark_unusable();
                }
            }
            b'}' | b']' if self.nested == 0 => {
                self.end_value(slot);
                self.phase = if byte == b'}' {
                    Phase::Finished
                } else {
                    Phase::Malformed
                };
            }
            b'}' | b']' => {
                self.nested = self.nested.saturating_sub(1);
                if self.nested == 0 {
                    self.phase = Phase::AfterValue;
                }
            }
            b',' if self.nested == 0 => {
                self.end_value(slot);
                self.phase = Phase::ExpectKey;
            }
            _ if self.nested == 0 && is_json_space(byte) => {
                self.end_value(slot);
                self.phase = Phase::AfterValue;
            }
            _ => {
                if capture && self.nested == 0 {
                    self.id.push(byte);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::lsp::RequestId;

    fn scan(line: &[u8]) -> Envelope {
        let mut scan = EnvelopeScan::default();
        scan.feed(line);
        scan.finish()
    }

    fn id_of(line: &[u8]) -> Option<Value> {
        scan(line).id.decode()
    }

    #[test]
    fn test_scan_finds_id_in_any_member_order() {
        let first = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{}}"#;
        let last = br#"{"jsonrpc":"2.0","method":"tools/call","params":{"x":[1,{"y":2}]},"id":8}"#;
        let spaced = b"{ \"id\" : 4 , \"method\" : \"m\" }\r";
        assert_eq!(id_of(first), Some(json!(7)));
        assert_eq!(id_of(last), Some(json!(8)));
        assert_eq!(id_of(spaced), Some(json!(4)));
        for line in [&first[..], &last[..], &spaced[..]] {
            let envelope = scan(line);
            assert_eq!(envelope.shape, Shape::Closed);
            assert!(envelope.method);
            assert!(!envelope.outcome);
        }
    }

    #[test]
    fn test_scan_ignores_nested_members() {
        let nested =
            br#"{"params":{"id":99,"method":"n","result":1,"s":"\"id\":5"},"jsonrpc":"2.0"}"#;
        let with_outer_id = br#"{"params":{"id":99,"a":[{"id":1}]},"method":"m","id":3}"#;
        let envelope = scan(nested);
        assert_eq!(envelope.shape, Shape::Closed);
        assert!(!envelope.method && !envelope.outcome);
        assert_eq!(envelope.id, IdToken::Absent);
        assert_eq!(id_of(with_outer_id), Some(json!(3)));
    }

    #[test]
    fn test_scan_flags_result_and_error_as_outcome() {
        for line in [
            &br#"{"id":1,"result":null}"#[..],
            &br#"{"id":1,"error":{"code":1}}"#[..],
            &br#"{"id":1,"result":1,"error":2}"#[..],
        ] {
            let envelope = scan(line);
            assert!(envelope.outcome && !envelope.method, "{line:?}");
        }
        assert!(!scan(br#"{"id":1,"results":1,"errors":2}"#).outcome);
    }

    #[test]
    fn test_scan_decodes_string_ids_with_escapes() {
        assert_eq!(
            id_of(br#"{"method":"m","id":"a\"b\\c"}"#),
            Some(json!("a\"b\\c"))
        );
    }

    #[test]
    fn test_scan_skips_a_leading_bom() {
        let mut line = BOM.to_vec();
        line.extend_from_slice(br#"{"id":2,"method":"m"}"#);
        assert_eq!(id_of(&line), Some(json!(2)));
    }

    #[test]
    fn test_scan_unusable_ids_are_present_but_not_decodable() {
        let long = format!(r#"{{"method":"m","id":"{}"}}"#, "i".repeat(MAX_ID_BYTES));
        let cases: [Vec<u8>; 6] = [
            br#"{"method":"m","id":{"a":1}}"#.to_vec(),
            br#"{"method":"m","id":[1]}"#.to_vec(),
            br#"{"method":"m","id":1,"id":2}"#.to_vec(),
            long.into_bytes(),
            br#"{"method":"m","id":"x"#.to_vec(),
            br#"{"method":"m","id":12"#.to_vec(),
        ];
        for case in cases {
            let envelope = scan(&case);
            assert_eq!(
                envelope.id,
                IdToken::Unusable,
                "{}",
                String::from_utf8_lossy(&case)
            );
        }
    }

    #[test]
    fn test_scan_id_just_under_the_length_cap_is_captured() {
        let id = "i".repeat(MAX_ID_BYTES - 2);
        let line = format!(r#"{{"method":"m","id":"{id}"}}"#);
        assert_eq!(id_of(line.as_bytes()), Some(json!(id)));
    }

    #[test]
    fn test_scan_shapes() {
        assert_eq!(scan(br#"{"a":1}"#).shape, Shape::Closed);
        assert_eq!(scan(br#"{"a":1} trailing"#).shape, Shape::Closed);
        let cases: [(&[u8], Shape); 7] = [
            (b"", Shape::Truncated),
            (b"{", Shape::Truncated),
            (br#"{"method":"m","id":1"#, Shape::Truncated),
            (b"not json at all", Shape::Malformed),
            (b"[1,2,3]", Shape::Malformed),
            (br#"{"method":"m","id" 1}"#, Shape::Malformed),
            (br#"{"a":1]"#, Shape::Malformed),
        ];
        for (case, shape) in cases {
            assert_eq!(scan(case).shape, shape, "{}", String::from_utf8_lossy(case));
        }
    }

    #[test]
    fn test_scan_id_cut_inside_the_token_is_never_complete() {
        assert_eq!(scan(br#"{"id":123"#).id, IdToken::Unusable);
        assert_eq!(
            scan(br#"{"id":123,"#).id,
            IdToken::Complete(b"123".to_vec())
        );
        assert_eq!(
            scan(br#"{"id":"ab""#).id,
            IdToken::Complete(b"\"ab\"".to_vec())
        );
    }

    #[test]
    fn test_scan_escaped_keys_are_not_recognized() {
        let method = scan(br#"{"id":1,"\u006dethod":"m"}"#);
        assert!(!method.method);
        let id = scan(b"{\"\\u0069d\":1,\"method\":\"m\"}");
        assert_eq!(id.id, IdToken::Absent);
        assert_eq!(
            id_of(br#"{"id":1,"\u0078":2,"method":"m"}"#),
            Some(json!(1))
        );
    }

    #[test]
    fn test_scan_result_is_independent_of_chunking() {
        let line =
            br#"{"jsonrpc":"2.0","method":"tools/call","params":{"a":"\"}","b":[1]},"id":"x\\y"}"#;
        let whole = scan(line);
        assert_eq!(whole.id.decode::<Value>(), Some(json!("x\\y")));
        for split in 0..=line.len() {
            let (head, tail) = line.split_at(split);
            let mut chunked = EnvelopeScan::default();
            chunked.feed(head);
            chunked.feed(tail);
            assert_eq!(chunked.finish(), whole, "split at {split}");
        }
    }

    #[test]
    fn test_scan_survives_every_prefix_and_junk_bytes() {
        let line = br#"{"jsonrpc":"2.0","method":"m","params":{"a":[1,"}"]},"id":5}"#;
        for end in 0..=line.len() {
            let _ = scan(&line[..end]);
        }
        for byte in 0..=u8::MAX {
            let _ = scan(&[b'{', b'"', b'i', b'd', b'"', b':', byte, byte, b'}', byte]);
        }
    }

    fn json_value() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;

        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            (-1.0e6_f64..1.0e6).prop_map(Value::from),
            "\\PC{0,8}".prop_map(Value::from),
        ];
        leaf.prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::vec(("[a-z]{0,4}", inner), 0..4)
                    .prop_map(|members| Value::Object(members.into_iter().collect())),
            ]
        })
    }

    proptest::proptest! {
        #[test]
        fn prop_scan_agrees_with_serde_json_on_generated_objects(
            id in proptest::option::of(json_value()),
            method in proptest::option::of(json_value()),
            outcome in proptest::option::of((proptest::bool::ANY, json_value())),
            extra in proptest::collection::vec(("[a-z]{1,4}", json_value()), 0..4),
            rotation in 0_usize..8,
            spacing in proptest::sample::select(vec!["", " ", "\t", "\r"]),
            split in 0_usize..400,
        ) {
            let mut object: serde_json::Map<String, Value> = extra.into_iter().collect();
            if let Some(id) = id {
                object.insert("id".to_owned(), id);
            }
            if let Some(method) = method {
                object.insert("method".to_owned(), method);
            }
            if let Some((is_result, value)) = outcome {
                object.insert(if is_result { "result" } else { "error" }.to_owned(), value);
            }
            let mut members: Vec<(String, Value)> =
                object.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let rotate_by = rotation.checked_rem(members.len().max(1)).unwrap_or(0);
            members.rotate_left(rotate_by);
            let body = members
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}{spacing}:{spacing}{}",
                        serde_json::to_string(k).unwrap(),
                        serde_json::to_string(v).unwrap()
                    )
                })
                .collect::<Vec<_>>()
                .join(&format!("{spacing},{spacing}"));
            let line = format!("{{{spacing}{body}{spacing}}}");

            let envelope = scan(line.as_bytes());
            proptest::prop_assert_eq!(envelope.shape, Shape::Closed);
            proptest::prop_assert_eq!(envelope.method, object.contains_key("method"));
            proptest::prop_assert_eq!(
                envelope.outcome,
                object.contains_key("result") || object.contains_key("error")
            );
            match object.get("id") {
                None => proptest::prop_assert_eq!(&envelope.id, &IdToken::Absent),
                Some(id) => proptest::prop_assert_eq!(
                    envelope.id.decode::<RequestId>(),
                    serde_json::from_value::<RequestId>(id.clone()).ok()
                ),
            }

            let cut = split.min(line.len());
            let (head, tail) = line.as_bytes().split_at(cut);
            let mut chunked = EnvelopeScan::default();
            chunked.feed(head);
            chunked.feed(tail);
            proptest::prop_assert_eq!(chunked.finish(), envelope);
        }

        #[test]
        fn prop_every_strict_prefix_is_truncated_and_never_misreports_the_id(
            id in json_value(),
            extra in proptest::collection::vec(("[a-z]{1,4}", json_value()), 0..4),
        ) {
            let mut object: serde_json::Map<String, Value> = extra.into_iter().collect();
            object.insert("method".to_owned(), json!("m"));
            object.insert("id".to_owned(), id.clone());
            let line = serde_json::to_string(&object).unwrap();

            let mut scan = EnvelopeScan::default();
            for (end, byte) in line.bytes().enumerate() {
                let envelope = scan.finish();
                proptest::prop_assert_eq!(envelope.shape, Shape::Truncated, "prefix {}", end);
                if let IdToken::Complete(raw) = &envelope.id {
                    proptest::prop_assert_eq!(raw, &serde_json::to_string(&id).unwrap().into_bytes());
                }
                scan.feed(&[byte]);
            }
            proptest::prop_assert_eq!(scan.finish().shape, Shape::Closed);
        }
    }
}
