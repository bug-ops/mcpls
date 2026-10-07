# Accessibility

`mcpls` is a command-line program and an MCP server. It has no graphical interface. This document states what that means for accessibility, what the project commits to, and how to report a barrier.

## Command-line output

- **Plain text on standard streams.** Diagnostic logs go to standard error, so standard output carries only the MCP protocol on the stdio transport. Both streams are plain text and work with screen readers and terminal braille displays.
- **No interactive prompts or full-screen UI.** `mcpls` does not draw a terminal interface and does not require a pointer or a TTY.
- **Machine-readable logs.** Set `--log-json` (or `MCPLS_LOG_JSON=true`) to emit JSON log lines instead of text. See [Command Line and Environment](https://bug-ops.github.io/mcpls/reference/cli.html).
- **Control-character safe logs.** Escape sequences and newlines in logged fields are escaped, so log text cannot corrupt terminal output or forge extra log lines.
- **Colour.** Text logs are produced by `tracing-subscriber`, which disables ANSI colour when the `NO_COLOR` environment variable is set to a non-empty value. `mcpls` has no other colour handling, and severity is always written as a word (`ERROR`, `WARN`, `INFO`), never signalled by colour alone.

## Tool results

Tool results are structured text returned to an MCP client. How they are presented, spoken, or magnified is up to the client. The project commits to keeping results self-describing: positions are 1-based line and column numbers, and severities are named, not encoded.

## Documentation

The user documentation is an [mdBook](https://rust-lang.github.io/mdBook/) in [`book/`](book/), published at <https://bug-ops.github.io/mcpls/>. Documentation contributions follow these rules:

- One H1 per page and no skipped heading levels, so heading navigation works in screen readers.
- Link text describes the destination. Avoid "here" and "click".
- Code samples use fenced blocks with a language tag. Tables hold short, factual cells and have a header row.
- Diagrams are accompanied by prose that states the same information. mdBook's built-in theme provides keyboard navigation, light and dark themes, and search.
- Information is never conveyed by colour alone.

This project does not claim conformance with WCAG or any other formal standard. The mdBook theme is not customised, so rendered pages inherit its behavior.

## Report an accessibility problem

Open an issue using the [bug report form](https://github.com/bug-ops/mcpls/issues/new?template=bug_report.yml) and state the assistive technology or terminal you use. For documentation barriers, name the page. Accessibility fixes in output and documentation are accepted as ordinary pull requests; see [CONTRIBUTING.md](CONTRIBUTING.md).
