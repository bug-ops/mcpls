# Introduction

mcpls gives an AI coding assistant the same understanding of your code that your editor has. Instead of reading source files as plain text, the assistant can ask a compiler-grade question and get a precise answer: what is the type of this variable, where is this function defined, who calls it, and what errors does the compiler report right now.

## The problem

An AI assistant without code intelligence works with text. To answer "where is `ApiError::Timeout` handled?" it searches for strings, guesses which matches are real, and misses the ones that go through aliases or re-exports.

Your editor does not guess. It talks to a **language server** (rust-analyzer, pyright, gopls, and others) over the Language Server Protocol (LSP), which knows the real structure of the project.

## What mcpls does

mcpls is a bridge. It speaks the Model Context Protocol (MCP) to your AI client and the Language Server Protocol (LSP) to the language servers:

```mermaid
flowchart LR
    C["AI client"] <-->|MCP| M["mcpls"] <-->|LSP| S["rust-analyzer / pyright / gopls / ..."]
```

It starts the language servers it needs for your project, translates every question into LSP, and returns the answer in a form the assistant can use. It exposes 31 tools, from `get_hover` and `get_references` to `rename_symbol` and `get_diagnostics`.

## What you get

- **Type information.** Ask what a variable or expression is, with documentation.
- **Cross-references.** Find every real usage of a symbol across the workspace.
- **Semantic navigation.** Jump to definitions, implementations, type definitions and declarations.
- **Real diagnostics.** Read the errors and warnings the compiler or linter reports, not guesses.
- **Safe refactoring.** Get a workspace-wide rename plan as a set of edits.

mcpls does not change your files. Tools such as `rename_symbol` and `format_document` return edits for the assistant to apply; every other tool is read-only.

## How this book is organized

The book follows a path from first use to deep detail. Read as far as you need.

| Part | Read it when you want to | Outcome |
|------|--------------------------|---------|
| [Part 1: Quick Start](getting-started/installation.md) | Try mcpls | A working setup and your first answer in minutes |
| [Part 2: Everyday Use](guide/configuration.md) | Use it on real projects | Configured servers, a tour of every tool, and fixes for common problems |
| [Part 3: Under the Hood](advanced/architecture.md) | Understand or harden a deployment | Architecture, positions, lifecycle, HTTP transport, security model |
| [Reference](reference/cli.md) | Look something up | Every flag, environment variable and configuration key |

## What's Next

Start with [Installation](getting-started/installation.md) to get the `mcpls` binary onto your machine.
