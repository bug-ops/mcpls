# Example Configuration

This page shows a complete, annotated `mcpls.toml` that you can copy and trim. Every line that is commented out is a working option you can enable by removing the `#`. Each key is described in the [Configuration Reference](config.md).

Save it as `mcpls.toml` in your [user config directory](cli.md#--config), or pass its path with `--config`. A `./mcpls.toml` inside a project is ignored unless you pass `--trust-project-config` ([Security and Trust](../advanced/security.md)).

```toml
{{#include example-config.toml}}
```

## What's Next

Learn what each key does in the [Configuration Reference](config.md), or return to [Configuration](../guide/configuration.md) for a guided walkthrough.
