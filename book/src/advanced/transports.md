# Transports: stdio and HTTP

In this chapter you learn the two ways a client can reach mcpls, when to use each, and how to expose the HTTP transport safely. Almost everyone uses stdio. Choose HTTP only when the client cannot launch a process or when several clients must share one mcpls.

## Prerequisites

- You know how a client [launches mcpls](../getting-started/connect-client.md).

## stdio (default)

The client starts `mcpls` as a child process and exchanges MCP messages on its standard input and output. Logs go to standard error so they never corrupt the stream. When the client exits and closes the pipe, mcpls shuts down and stops its language servers.

stdio needs no network, no authentication and no ports, which is why it is the default and the safest choice.

## HTTP (Streamable HTTP)

With HTTP, mcpls binds a TCP address and serves MCP 2025-11-25 Streamable HTTP. Several clients can connect, and mcpls runs independently of them.

### Enabling the HTTP transport

HTTP is an optional Cargo feature, `transport-http`. The prebuilt release binaries and the Docker image are built without it, so `--listen` does not exist in them. Build it yourself:

```bash
cargo install mcpls --features transport-http
```

or, from a checkout:

```bash
cargo install --path crates/mcpls-cli --features transport-http
```

### Start it

```bash
mcpls --listen 127.0.0.1:3000
```

The service is mounted at `/mcp` and also at `/`. A reverse-proxy rule must cover both. Change the mount point with `--http-path`:

```bash
mcpls --listen 127.0.0.1:3000 --http-path /api/mcp
```

The path must start with `/`, must not be `/`, must have no empty, `.` or `..` segment, and may use only ASCII letters, digits and `-._~`. An invalid value exits with a usage error (code 2) before any language server starts, even without `--listen`.

Every HTTP option also exists as an environment variable: `MCPLS_LISTEN`, `MCPLS_HTTP_PATH`, `MCPLS_HTTP_STREAM_LIVENESS`, `MCPLS_HTTP_ALLOWED_ORIGINS` and `MCPLS_HTTP_ALLOWED_HOSTS` ([reference](../reference/cli.md)).

### There is no authentication

mcpls performs no authentication on any transport. Binding to a loopback address limits access to the local machine. Binding to anything else, such as `0.0.0.0`, requires a reverse proxy that authenticates requests before forwarding them. Example with nginx:

```nginx
location / {
    auth_request /auth;
    proxy_pass http://127.0.0.1:3000;
    proxy_set_header Host localhost;
}
```

The `Host` line matters, as the next section explains.

## Host and Origin allowlists

Browsers can be tricked into sending requests to local services (DNS rebinding). mcpls checks two headers on every HTTP request, `Host` first and then `Origin`.

### Host

The `Host` header must be one of:

- `localhost`, `127.0.0.1` or `::1`, with any port;
- the bound IP address, when you bound a specific non-loopback address;
- a value you list with `--http-allowed-host`.

There is no wildcard. A bind to `0.0.0.0` therefore allows only the loopback names plus the hosts you list, so clients that reach it by another name need that name listed, or a proxy that rewrites `Host`.

```bash
mcpls --listen 0.0.0.0:8443 --http-allowed-host mcp.example.com:8443
```

A value is a host name or IP address with an optional port. Without a port any port matches. With one, only that port matches, and a request that omits the port does not. Clients omit `:80` and `:443`, so pinning either, or port 0, is a usage error: list the host without a port. Wildcards, user information, a scheme, a path, an empty port, a trailing dot, a non-ASCII character (write it in punycode) and an unbracketed IPv6 address are usage errors too. Matching ignores case. Repeat the flag or separate values with commas.

### Origin

A request that carries an `Origin` header, which browsers add, is accepted only when the origin is `localhost`, `127.0.0.1` or `[::1]` on the bound port, or one you list with `--http-allowed-origin`. Anything else, including `Origin: null`, is answered with `403`. Requests without `Origin`, which includes every non-browser client, are not affected.

```bash
mcpls --listen 127.0.0.1:3000 \
  --http-allowed-origin https://app.example.com \
  --http-allowed-origin http://[::1]:8080
```

Each value is `http://` or `https://`, a host and an optional port; a missing port means the scheme default (80 or 443). A path, a query, user information, a wildcard, `null`, an invalid port and an unbracketed IPv6 host are usage errors.

Allowed origins do not relax the `Host` check, which runs first. A browser page that reaches mcpls by a non-loopback name needs that name in `--http-allowed-host` as well.

## Limits and timeouts

The HTTP server bounds slow and vanished clients. The defaults are:

| Limit | Default | Effect |
|-------|---------|--------|
| Request body | 4 MiB | Larger requests receive `413` |
| Header read and body-chunk pause | 30 s | A slow request head or body is answered with `408`, and idle keep-alive connections close |
| Write stall | 30 s | A connection whose writes make no progress is closed, which frees a peer that stopped reading |
| Concurrent connections | 512 | Raise `ulimit -n` first on macOS, where launchd's soft limit is 256 |
| Concurrent sessions | 100 | New sessions beyond the limit receive `429` |
| Session idle timeout | 5 minutes | A session with no inbound request expires |
| Response stream deadline | 1 hour | A POST or resume response stream is cut, so its session can expire |

Embedders of `mcpls-core` can change these through `HttpConfig`. On the command line only the options listed in the [reference](../reference/cli.md) are available.

### Liveness and leases

mcpls probes each session's standalone GET (SSE) stream. With `--http-stream-liveness probe` (the default), it sends an MCP `ping` every 60 seconds and closes the stream if the client does not answer, by POSTing the JSON-RPC response, within 30 seconds. This frees the stream of a vanished client, such as a sleeping laptop, and works behind a reverse proxy. A client that answers probes keeps its session until it closes the stream or sends `DELETE`.

A client that ignores server `ping` requests would be disconnected every 90 seconds. For such a client use `off`:

```bash
mcpls --listen 127.0.0.1:3000 --http-stream-liveness off
```

With `off` there is no proof of life, so an open stream does not hold its session: the session expires after 5 minutes without an inbound request even while notifications flow, and the client must send a request, such as `ping`, more often than that. `off` also disables the 15 to 30 minute lease on stateless `subscriptions/listen` streams ([Diagnostics and Resources](diagnostics.md#subscriptions)).

On Linux and Android, mcpls also sets `TCP_USER_TIMEOUT` (60 seconds) on accepted sockets. The probe is the portable complement, because the kernel timeout does not see through a reverse proxy.

## Logging and session ids

The HTTP session id is a bearer secret. mcpls caps the log level of the `rmcp` targets that print it and logs only a short hash of the id. A more specific `MCPLS_LOG` directive for those targets overrides the cap and puts session ids back into the logs.

## What's Next

Continue with [Security and Trust](security.md) to learn what mcpls protects against and what it deliberately does not.
