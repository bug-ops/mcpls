# ADR-004: Position Encoding Strategy

**Status**: ACCEPTED

**Date**: 2025-12-24

## Context

LSP 3.17 allows negotiation of position encoding during initialization:
- UTF-8: Code units in UTF-8 encoding
- UTF-16: Code units in UTF-16 encoding (default, mandatory)
- UTF-32: Unicode code points

MCP tools use 1-based positions for human readability.
LSP uses 0-based positions internally.

Multibyte characters (emoji, CJK) require proper encoding handling.

## Decision

1. **Prefer UTF-8** encoding when negotiating with LSP servers
2. **Fall back to UTF-16** if server doesn't support UTF-8
3. **Convert positions** at MCP/LSP boundary:
   - MCP (1-based) → LSP (0-based) on request
   - LSP (0-based) → MCP (1-based) on response

```rust
// Advertise preference during initialization
InitializeParams {
    capabilities: ClientCapabilities {
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![
                PositionEncodingKind::UTF8,   // Preferred
                PositionEncodingKind::UTF16,  // Mandatory fallback
            ]),
            ..Default::default()
        }),
        ..Default::default()
    },
    ..Default::default()
}

// Position conversion
fn mcp_to_lsp_position(line: u32, character: u32) -> Position {
    Position {
        line: line.saturating_sub(1),
        character: character.saturating_sub(1),
    }
}
```

## Consequences

### Positive

- Native Rust UTF-8 operations when possible (no conversion overhead)
- Correct handling of multibyte characters
- Human-readable positions in MCP responses
- Compliant with LSP 3.17 specification

### Negative

- Must handle encoding conversion logic
- UTF-16 fallback requires character counting in different encoding
- Edge cases with invalid positions

## Alternatives Considered

### UTF-16 only

Rejected because:
- Inefficient for Rust strings (native UTF-8)
- Extra conversion for every position
- Most modern servers support UTF-8

### UTF-32 preferred

Rejected because:
- Rare server support
- Memory overhead
- No significant benefit over UTF-8

## References

- [LSP 3.17 - Position Encoding](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#positionEncodingKind)
- [Rust String UTF-8](https://doc.rust-lang.org/std/string/struct.String.html)
