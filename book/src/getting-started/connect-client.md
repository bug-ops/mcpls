# Connect an AI Client

In this chapter you register mcpls with your AI client. The client launches `mcpls` as a child process and talks to it over standard input and output (the stdio transport), so there is nothing to run by hand.

## Prerequisites

- `mcpls --version` works in a terminal ([Installation](installation.md)).
- A language server for your project is installed.

## Claude Code

Register mcpls once for all your projects:

```bash
claude mcp add --scope user mcpls -- mcpls
```

Everything after `--` is the command Claude Code runs. Confirm the registration:

```bash
claude mcp list
```

The list shows `mcpls` with the command `mcpls`. Start a new Claude Code session inside your project to load it.

Claude Code starts the server from your project directory, and mcpls uses that directory as the workspace.

### Share it with a team

To commit the registration to a repository, use the project scope, which writes a `.mcp.json` file at the repository root:

```bash
claude mcp add --scope project mcpls -- mcpls
```

The file contains:

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "mcpls",
      "args": []
    }
  }
}
```

> **Important:** A project-scoped file is controlled by the repository. If you analyze code you do not trust, register mcpls in your user scope instead, so the repository cannot change how mcpls is launched. See [Security and Trust](../advanced/security.md).

## Claude Desktop and other clients

Any MCP client that can launch a stdio server works with the same entry. For Claude Desktop, edit `claude_desktop_config.json`:

- macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
- Windows: `%APPDATA%\Claude\claude_desktop_config.json`

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "mcpls",
      "args": []
    }
  }
}
```

JSON does not allow trailing commas; a stray one prevents the client from reading the file. Restart the client after saving.

A desktop application may not inherit your shell `PATH`. If it cannot find `mcpls`, use the absolute path, for example `"command": "/Users/you/.local/bin/mcpls"`. Find it with `which mcpls`.

Desktop clients do not always start servers in a project directory. Set the workspace explicitly with `args` and a configuration file, as described in [Minimal Configuration](minimal-config.md).

## Verify the connection

Ask the assistant:

> Which mcpls tools do you have?

It lists 31 tools such as `get_hover`, `get_definition`, `get_references` and `get_diagnostics`. If the list is empty, see [Troubleshooting](../guide/troubleshooting.md#mcpls-does-not-appear-in-the-client).

## What's Next

With the connection working, [ask your first question](first-query.md) about real code.
