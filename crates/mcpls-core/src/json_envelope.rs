//! Constant-memory scan of the top-level members of one JSON-RPC envelope.
//!
//! Both the stdio transport (a line the MCP codec rejected or the size limit
//! refused) and the LSP transport (a frame skipped for size or that failed to
//! decode) must tell what a message was, and whom it addresses, without
//! building it. [`EnvelopeScan`] reads the bytes once, in any chunking, and
//! keeps only whether `result` or `error` appeared and bounded copies of the
//! `id` and `method` tokens. [`EnvelopeScan::with_params_uri`] also keeps the
//! `uri` member of an object-valued `params`.
//!
//! [`MessageKind`] is the one rule that turns the members a message carries
//! into what it is; a decoded [`serde_json::Value`] and a scanned envelope are
//! classified by the same table.
//!
//! A key written with escapes (`"id"`) is never matched by the scanner,
//! while a decoded `Value` has it unescaped, so the two readers differ on such
//! a message. The scan is a floor for frames that cannot be decoded, not a
//! second JSON parser.

use serde::de::DeserializeOwned;
use serde_json::Value;

const MAX_ID_BYTES: usize = 128;
const MAX_METHOD_BYTES: usize = 128;
/// Longest `uri` a lost `publishDiagnostics` can name, in bytes. A longer one
/// is treated as unidentified.
pub const MAX_LOST_URI_BYTES: usize = 16 * 1024;
const QUOTES: usize = 2;
const KEY_BYTES: usize = 6;
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Which of the JSON-RPC members a message carries; `outcome` is `result` or
/// `error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Members {
    pub(crate) method: bool,
    pub(crate) id: bool,
    pub(crate) outcome: bool,
}

impl Members {
    /// The members of a decoded message, a key counting as present whatever its
    /// value (`null` included).
    pub(crate) fn of_value(value: &Value) -> Self {
        let has = |key: &str| value.get(key).is_some();
        Self {
            method: has("method"),
            id: has("id"),
            outcome: has("result") || has("error"),
        }
    }
}

/// What a JSON-RPC message is, decided from the members it carries alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Request,
    Notification,
    Response,
    /// An `id` with neither `result` nor `error` and no `method`.
    IncompleteResponse,
    Unrecognized,
}

impl MessageKind {
    /// The one classification table.
    pub(crate) const fn of(members: Members) -> Self {
        match (members.method, members.id, members.outcome) {
            (true, true, _) => Self::Request,
            (true, false, _) => Self::Notification,
            (false, true, true) => Self::Response,
            (false, true, false) => Self::IncompleteResponse,
            (false, false, _) => Self::Unrecognized,
        }
    }
}

/// A key, kept only as far as it can still equal a member of interest.
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

    /// The key's bytes, empty when it was written with escapes or is too long
    /// to equal any member of interest.
    fn matchable_bytes(&self) -> &[u8] {
        if self.matchable {
            self.bytes.get(..self.len).unwrap_or_default()
        } else {
            &[]
        }
    }
}

/// The raw token of one member's value, bounded.
#[derive(Debug)]
struct Capture {
    state: CaptureState,
    limit: usize,
}

#[derive(Debug)]
enum CaptureState {
    Absent,
    /// The value has begun but its end was not seen, so it may be cut short.
    Partial(Vec<u8>),
    Complete(Vec<u8>),
    Unusable,
}

impl Capture {
    const fn new(limit: usize) -> Self {
        Self {
            state: CaptureState::Absent,
            limit,
        }
    }

    fn begin(&mut self) {
        self.state = match self.state {
            CaptureState::Absent => CaptureState::Partial(Vec::new()),
            CaptureState::Partial(_) | CaptureState::Complete(_) | CaptureState::Unusable => {
                CaptureState::Unusable
            }
        };
    }

    fn push(&mut self, byte: u8) {
        if let CaptureState::Partial(raw) = &mut self.state {
            if raw.len() < self.limit {
                raw.push(byte);
            } else {
                self.state = CaptureState::Unusable;
            }
        }
    }

    fn complete(&mut self) {
        if let CaptureState::Partial(raw) = &mut self.state {
            self.state = CaptureState::Complete(std::mem::take(raw));
        }
    }

    fn mark_unusable(&mut self) {
        self.state = CaptureState::Unusable;
    }

    fn token(&self) -> Token {
        match &self.state {
            CaptureState::Absent => Token::Absent,
            CaptureState::Complete(raw) => Token::Complete(raw.clone()),
            CaptureState::Partial(_) | CaptureState::Unusable => Token::Unusable,
        }
    }
}

/// The members one scan reads, and what it keeps of each.
trait Layout {
    type Slot: Copy + Eq + std::fmt::Debug;

    fn slot_of(key: &KeyBuf) -> Self::Slot;

    /// A value of `slot` begins.
    fn begin(&mut self, slot: Self::Slot);

    /// Where the value of `slot` is captured, if it is.
    fn capture(&mut self, slot: Self::Slot) -> Option<&mut Capture>;

    /// One byte of the value of `slot`, whatever its nesting.
    fn value_byte(&mut self, _slot: Self::Slot, _byte: u8) {}
}

#[derive(Debug)]
enum Phase<S> {
    BeforeRoot,
    ExpectKey,
    InKey(KeyBuf),
    AfterKey(S),
    ValueStart(S),
    InValue(S),
    AfterValue,
    Finished,
    Malformed,
}

/// Streaming reader of the members of one JSON object, as `L` lays them out.
#[derive(Debug)]
struct ObjectScan<L: Layout> {
    phase: Phase<L::Slot>,
    nested: usize,
    in_string: bool,
    escaped: bool,
    layout: L,
}

const fn is_json_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

impl<L: Layout> ObjectScan<L> {
    const fn new(layout: L) -> Self {
        Self {
            phase: Phase::BeforeRoot,
            nested: 0,
            in_string: false,
            escaped: false,
            layout,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if matches!(self.phase, Phase::Finished | Phase::Malformed) {
                return;
            }
            self.step(byte);
        }
    }

    const fn shape(&self) -> Shape {
        match self.phase {
            Phase::Finished => Shape::Closed,
            Phase::Malformed => Shape::Malformed,
            _ => Shape::Truncated,
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
                            let slot = L::slot_of(key);
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

    fn begin_value(&mut self, slot: L::Slot) {
        self.nested = 0;
        self.in_string = false;
        self.escaped = false;
        self.layout.begin(slot);
    }

    fn end_value(&mut self, slot: L::Slot) {
        if let Some(capture) = self.layout.capture(slot) {
            capture.complete();
        }
    }

    fn capture_byte(&mut self, slot: L::Slot, byte: u8) {
        if let Some(capture) = self.layout.capture(slot) {
            capture.push(byte);
        }
    }

    fn value_byte(&mut self, slot: L::Slot, byte: u8) {
        self.layout.value_byte(slot, byte);
        if self.in_string {
            self.capture_byte(slot, byte);
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
                self.capture_byte(slot, byte);
            }
            b'{' | b'[' => {
                self.nested = self.nested.saturating_add(1);
                if let Some(capture) = self.layout.capture(slot) {
                    capture.mark_unusable();
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
                if self.nested == 0 {
                    self.capture_byte(slot, byte);
                }
            }
        }
    }
}

/// The members of an envelope the scan reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvelopeSlot {
    Id,
    Method,
    Result,
    Error,
    Params,
    Other,
}

/// The `uri` of the `params` object, read by an inner scan of the `params` bytes.
#[derive(Debug)]
enum ParamsUri {
    Off,
    Absent,
    Active(Box<ObjectScan<UriLayout>>),
    /// `params` appeared twice.
    Unusable,
}

#[derive(Debug)]
struct EnvelopeLayout {
    id: Capture,
    method: Capture,
    outcome: bool,
    params: ParamsUri,
}

impl Layout for EnvelopeLayout {
    type Slot = EnvelopeSlot;

    fn slot_of(key: &KeyBuf) -> EnvelopeSlot {
        match key.matchable_bytes() {
            b"id" => EnvelopeSlot::Id,
            b"method" => EnvelopeSlot::Method,
            b"result" => EnvelopeSlot::Result,
            b"error" => EnvelopeSlot::Error,
            b"params" => EnvelopeSlot::Params,
            _ => EnvelopeSlot::Other,
        }
    }

    fn begin(&mut self, slot: EnvelopeSlot) {
        match slot {
            EnvelopeSlot::Id => self.id.begin(),
            EnvelopeSlot::Method => self.method.begin(),
            EnvelopeSlot::Result | EnvelopeSlot::Error => self.outcome = true,
            EnvelopeSlot::Params => match self.params {
                ParamsUri::Off => {}
                ParamsUri::Absent => {
                    self.params = ParamsUri::Active(Box::new(ObjectScan::new(UriLayout::new())));
                }
                ParamsUri::Active(_) | ParamsUri::Unusable => self.params = ParamsUri::Unusable,
            },
            EnvelopeSlot::Other => {}
        }
    }

    fn capture(&mut self, slot: EnvelopeSlot) -> Option<&mut Capture> {
        match slot {
            EnvelopeSlot::Id => Some(&mut self.id),
            EnvelopeSlot::Method => Some(&mut self.method),
            EnvelopeSlot::Result
            | EnvelopeSlot::Error
            | EnvelopeSlot::Params
            | EnvelopeSlot::Other => None,
        }
    }

    fn value_byte(&mut self, slot: EnvelopeSlot, byte: u8) {
        if let (EnvelopeSlot::Params, ParamsUri::Active(inner)) = (slot, &mut self.params) {
            inner.feed(&[byte]);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UriSlot {
    Uri,
    Other,
}

#[derive(Debug)]
struct UriLayout {
    uri: Capture,
}

impl UriLayout {
    const fn new() -> Self {
        Self {
            uri: Capture::new(MAX_LOST_URI_BYTES + QUOTES),
        }
    }
}

impl Layout for UriLayout {
    type Slot = UriSlot;

    fn slot_of(key: &KeyBuf) -> UriSlot {
        match key.matchable_bytes() {
            b"uri" => UriSlot::Uri,
            _ => UriSlot::Other,
        }
    }

    fn begin(&mut self, slot: UriSlot) {
        if slot == UriSlot::Uri {
            self.uri.begin();
        }
    }

    fn capture(&mut self, slot: UriSlot) -> Option<&mut Capture> {
        (slot == UriSlot::Uri).then_some(&mut self.uri)
    }
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

/// What the scan saw of one member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// No such member.
    Absent,
    /// The whole value token, within its bound, ended before the input did.
    Complete(Vec<u8>),
    /// Present but not capturable: nested, repeated, too long or cut short.
    Unusable,
}

impl Token {
    /// Whether the member exists, whatever its value.
    pub(crate) const fn is_present(&self) -> bool {
        !matches!(self, Self::Absent)
    }

    /// The value as `T`, `None` when absent, unusable or not a `T`.
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
    /// What the scan saw of the `method` member.
    pub(crate) method: Token,
    /// What the scan saw of the `id` member.
    pub(crate) id: Token,
    /// What the scan saw of the `uri` member of an object `params`; always
    /// absent unless the scan was made with [`EnvelopeScan::with_params_uri`].
    pub(crate) params_uri: Token,
    outcome: bool,
}

impl Envelope {
    /// What the message is, by the members the scan found.
    pub(crate) const fn kind(&self) -> MessageKind {
        MessageKind::of(Members {
            method: self.method.is_present(),
            id: self.id.is_present(),
            outcome: self.outcome,
        })
    }
}

/// Streaming scanner for the top-level `id`, `method`, `result`, `error` and
/// (optionally) `params.uri` members of one JSON object, in constant memory
/// whatever the object's size.
///
/// It never panics on malformed input and never keeps more than 128 bytes of
/// the id or the method, or 16 KiB of the uri. Members are captured only at
/// their own depth, so a nested `id` inside `params` is never mistaken for the
/// envelope's. A leading byte order mark is skipped and member order does not
/// matter. A key written with escapes is never matched. A member that appears
/// twice is present but unusable. Bytes after the closing brace are ignored.
#[derive(Debug)]
pub struct EnvelopeScan(ObjectScan<EnvelopeLayout>);

impl Default for EnvelopeScan {
    /// A scan of the envelope members alone.
    fn default() -> Self {
        Self::with_params(ParamsUri::Off)
    }
}

impl EnvelopeScan {
    /// A scan that also reads the `uri` of an object-valued `params`.
    pub(crate) const fn with_params_uri() -> Self {
        Self::with_params(ParamsUri::Absent)
    }

    const fn with_params(params: ParamsUri) -> Self {
        Self(ObjectScan::new(EnvelopeLayout {
            id: Capture::new(MAX_ID_BYTES),
            method: Capture::new(MAX_METHOD_BYTES),
            outcome: false,
            params,
        }))
    }

    /// Reads the next chunk of the input.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.0.feed(bytes);
    }

    /// What the bytes fed so far amount to.
    pub(crate) fn finish(&self) -> Envelope {
        let layout = &self.0.layout;
        let params_uri = match &layout.params {
            ParamsUri::Off | ParamsUri::Absent => Token::Absent,
            ParamsUri::Unusable => Token::Unusable,
            ParamsUri::Active(inner) => inner.layout.uri.token(),
        };
        Envelope {
            shape: self.0.shape(),
            method: layout.method.token(),
            id: layout.id.token(),
            params_uri,
            outcome: layout.outcome,
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
    fn test_message_kind_of_a_decoded_value_is_decided_by_the_keys_alone() {
        let kind = |value: serde_json::Value| MessageKind::of(Members::of_value(&value));

        assert_eq!(
            kind(serde_json::json!({"method": "m", "id": 1})),
            MessageKind::Request
        );
        assert_eq!(
            kind(serde_json::json!({"method": "m"})),
            MessageKind::Notification
        );
        assert_eq!(
            kind(serde_json::json!({"id": 1, "result": null})),
            MessageKind::Response
        );
        assert_eq!(
            kind(serde_json::json!({"id": 1, "error": {}})),
            MessageKind::Response
        );
        assert_eq!(
            kind(serde_json::json!({"id": 1})),
            MessageKind::IncompleteResponse
        );
        assert_eq!(
            kind(serde_json::json!({"result": 1})),
            MessageKind::Unrecognized
        );
        assert_eq!(
            kind(serde_json::json!({"method": "m", "id": 1, "result": 1})),
            MessageKind::Request
        );
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
            assert_eq!(envelope.kind(), MessageKind::Request);
        }
    }

    #[test]
    fn test_scan_ignores_nested_members() {
        let nested =
            br#"{"params":{"id":99,"method":"n","result":1,"s":"\"id\":5"},"jsonrpc":"2.0"}"#;
        let with_outer_id = br#"{"params":{"id":99,"a":[{"id":1}]},"method":"m","id":3}"#;
        let envelope = scan(nested);
        assert_eq!(envelope.shape, Shape::Closed);
        assert_eq!(envelope.kind(), MessageKind::Unrecognized);
        assert_eq!(envelope.id, Token::Absent);
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
            assert_eq!(envelope.kind(), MessageKind::Response, "{line:?}");
        }
        assert_eq!(
            scan(br#"{"id":1,"results":1,"errors":2}"#).kind(),
            MessageKind::IncompleteResponse
        );
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
                Token::Unusable,
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
        assert_eq!(scan(br#"{"id":123"#).id, Token::Unusable);
        assert_eq!(scan(br#"{"id":123,"#).id, Token::Complete(b"123".to_vec()));
        assert_eq!(
            scan(br#"{"id":"ab""#).id,
            Token::Complete(b"\"ab\"".to_vec())
        );
    }

    #[test]
    fn test_scan_escaped_keys_are_not_recognized() {
        let method = scan(br#"{"id":1,"\u006dethod":"m"}"#);
        assert_eq!(method.kind(), MessageKind::IncompleteResponse);
        let id = scan(b"{\"\\u0069d\":1,\"method\":\"m\"}");
        assert_eq!(id.id, Token::Absent);
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
    fn test_params_uri_is_read_beside_the_envelope_members() {
        let line = br#"{"method":"m","params":{"uri":"x"},"id":1}"#;
        let envelope = scan_with_params_uri(line);
        assert_eq!(envelope.id.decode::<Value>(), Some(json!(1)));
        assert_eq!(envelope.params_uri.decode::<String>(), Some("x".to_owned()));
        assert_eq!(envelope.kind(), MessageKind::Request);
        assert_eq!(scan(line).params_uri, Token::Absent);
    }

    #[test]
    fn test_params_uri_is_taken_at_depth_one_only() {
        let nested = br#"{"method":"m","params":{"a":{"uri":"deep"},"b":[{"uri":"in-array"}]}}"#;
        let after_nested = br#"{"method":"m","params":{"a":{"uri":"deep"},"uri":"top"}}"#;
        let not_object = [
            &br#"{"method":"m","params":[{"uri":"x"}]}"#[..],
            &br#"{"method":"m","params":"uri"}"#[..],
            &br#"{"method":"m","params":null}"#[..],
            &br#"{"method":"m","params":{}}"#[..],
            &br#"{"method":"m"}"#[..],
        ];
        assert_eq!(scan_with_params_uri(nested).params_uri, Token::Absent);
        assert_eq!(
            scan_with_params_uri(after_nested)
                .params_uri
                .decode::<String>(),
            Some("top".to_owned())
        );
        for line in not_object {
            assert_eq!(
                scan_with_params_uri(line).params_uri,
                Token::Absent,
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn test_repeated_params_or_uri_keys_make_the_uri_unusable() {
        let cases = [
            &br#"{"method":"m","params":{"uri":"a","uri":"b"}}"#[..],
            &br#"{"method":"m","params":{"uri":"a"},"params":{"uri":"a"}}"#[..],
        ];
        for line in cases {
            let envelope = scan_with_params_uri(line);
            assert_eq!(envelope.params_uri, Token::Unusable);
            assert_eq!(envelope.params_uri.decode::<String>(), None);
        }
    }

    #[test]
    fn test_an_escaped_uri_key_is_not_recognized() {
        let line = b"{\"method\":\"m\",\"params\":{\"\\u0075ri\":\"x\"}}";
        assert_eq!(scan_with_params_uri(line).params_uri, Token::Absent);
    }

    #[test]
    fn test_params_uri_over_its_bound_is_unusable_and_a_cut_one_is_never_complete() {
        let at_bound = "u".repeat(MAX_LOST_URI_BYTES);
        let over_bound = "u".repeat(MAX_LOST_URI_BYTES + 1);
        let line = |uri: &str| format!(r#"{{"method":"m","params":{{"uri":"{uri}"}}}}"#);
        assert_eq!(
            scan_with_params_uri(line(&at_bound).as_bytes())
                .params_uri
                .decode::<String>(),
            Some(at_bound)
        );
        assert_eq!(
            scan_with_params_uri(line(&over_bound).as_bytes()).params_uri,
            Token::Unusable
        );
        assert_eq!(
            scan_with_params_uri(br#"{"method":"m","params":{"uri":"fil"#).params_uri,
            Token::Unusable
        );
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

    /// A `params` value: anything, or an object whose `uri` sits beside decoy
    /// `uri` and `params` keys at deeper levels.
    fn params_value() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;

        prop_oneof![
            json_value(),
            (
                proptest::option::of(json_value()),
                prop::collection::vec(("(uri|params|[a-z]{0,4})", json_value()), 0..4),
            )
                .prop_map(|(uri, extra)| {
                    let mut members: serde_json::Map<String, Value> = extra.into_iter().collect();
                    if let Some(uri) = uri {
                        members.insert("uri".to_owned(), uri);
                    }
                    Value::Object(members)
                }),
        ]
    }

    fn scan_with_params_uri(line: &[u8]) -> Envelope {
        let mut scan = EnvelopeScan::with_params_uri();
        scan.feed(line);
        scan.finish()
    }

    /// What the scan must make of `object.params.uri`.
    fn expected_params_uri(object: &serde_json::Map<String, Value>) -> Token {
        match object.get("params").and_then(|params| params.get("uri")) {
            None => Token::Absent,
            Some(Value::Object(_) | Value::Array(_)) => Token::Unusable,
            Some(uri) => Token::Complete(serde_json::to_string(uri).unwrap().into_bytes()),
        }
    }

    proptest::proptest! {
        #[test]
        fn prop_scan_agrees_with_serde_json_on_generated_objects(
            id in proptest::option::of(json_value()),
            method in proptest::option::of(json_value()),
            outcome in proptest::option::of((proptest::bool::ANY, json_value())),
            params in proptest::option::of(params_value()),
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
            if let Some(params) = params {
                object.insert("params".to_owned(), params);
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
            proptest::prop_assert_eq!(envelope.method.is_present(), object.contains_key("method"));
            proptest::prop_assert_eq!(
                envelope.outcome,
                object.contains_key("result") || object.contains_key("error")
            );
            match object.get("id") {
                None => proptest::prop_assert_eq!(&envelope.id, &Token::Absent),
                Some(id) => proptest::prop_assert_eq!(
                    envelope.id.decode::<RequestId>(),
                    serde_json::from_value::<RequestId>(id.clone()).ok()
                ),
            }

            proptest::prop_assert_eq!(&envelope.params_uri, &Token::Absent);
            let with_uri = scan_with_params_uri(line.as_bytes());
            proptest::prop_assert_eq!(with_uri.shape, Shape::Closed);
            proptest::prop_assert_eq!(&with_uri.id, &envelope.id);
            proptest::prop_assert_eq!(&with_uri.method, &envelope.method);
            proptest::prop_assert_eq!(with_uri.kind(), envelope.kind());
            proptest::prop_assert_eq!(&with_uri.params_uri, &expected_params_uri(&object));

            let cut = split.min(line.len());
            let (head, tail) = line.as_bytes().split_at(cut);
            let mut chunked = EnvelopeScan::default();
            chunked.feed(head);
            chunked.feed(tail);
            proptest::prop_assert_eq!(chunked.finish(), envelope);
            let mut chunked = EnvelopeScan::with_params_uri();
            chunked.feed(head);
            chunked.feed(tail);
            proptest::prop_assert_eq!(chunked.finish(), with_uri);
        }

        #[test]
        fn prop_a_complete_params_uri_of_a_prefix_equals_the_full_one(
            params in params_value(),
            extra in proptest::collection::vec(("[a-z]{1,4}", json_value()), 0..4),
        ) {
            let mut object: serde_json::Map<String, Value> = extra.into_iter().collect();
            object.insert("method".to_owned(), json!("m"));
            object.insert("params".to_owned(), params);
            let line = serde_json::to_string(&object).unwrap();
            let full = expected_params_uri(&object);

            let mut scan = EnvelopeScan::with_params_uri();
            for byte in line.bytes() {
                if let Token::Complete(raw) = scan.finish().params_uri {
                    proptest::prop_assert_eq!(&Token::Complete(raw), &full);
                }
                scan.feed(&[byte]);
            }
            proptest::prop_assert_eq!(scan.finish().params_uri, full);
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
                if let Token::Complete(raw) = &envelope.id {
                    proptest::prop_assert_eq!(raw, &serde_json::to_string(&id).unwrap().into_bytes());
                }
                scan.feed(&[byte]);
            }
            proptest::prop_assert_eq!(scan.finish().shape, Shape::Closed);
        }
    }
}
