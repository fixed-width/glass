# Run glass over the network

stdio requires `glass-mcp` to run on the **same machine** as the agent. When the agent and the target
app are on different machines, run `glass-mcp` as a network server on the app's machine (rmcp
Streamable HTTP) and point your client at the URL.

| Situation | Transport | How |
|---|---|---|
| Agent + app on the same machine | stdio | Register `glass-mcp`; the client spawns it. Zero config — see [connect-an-agent.md](connect-an-agent.md). |
| Agent and app on different machines | network (HTTP) | Run `glass-mcp serve --http --addr …` on the app's machine; point the client at the URL with a bearer token. |

The network transport is behind the default-on `network` cargo feature; a `--no-default-features`
build is stdio-only.

## With a bearer token (non-loopback bind)

Generate a token, then serve with it:

```bash
mkdir -p ~/.glass
glass-mcp gen-token --out ~/.glass/token                  # cross-platform CSPRNG token
glass-mcp serve --http --addr 0.0.0.0:7300 --token-file ~/.glass/token
```

The client supplies the token as an `Authorization: Bearer <token>` header (check your client's docs
for its bearer-token / headers field). Binding a non-loopback address **without** a token is refused
at startup — fail-closed. You can pass the token via `GLASS_TOKEN` instead of `--token-file`.

> **Token-file permissions on Windows.** On Linux, glass forces the token file to owner-only
> (`0600`). On Windows the file inherits the **permissions of the folder** you write it into; glass
> does not yet set an explicit owner-only ACL. Keep `--out` inside your per-user profile (e.g.
> `%USERPROFILE%\.glass`), whose default permissions already restrict it to you, SYSTEM, and
> Administrators. **Don't** write it to a shared, world-readable, or cloud-synced (OneDrive-backed)
> folder — or skip the file and pass the token via `GLASS_TOKEN`.

## Over an SSH tunnel (no token)

A loopback bind needs no token and pairs with an SSH tunnel for confidentiality. On the agent's
machine, forward the remote port to your local loopback; on the app's machine, bind loopback only:

```bash
# agent's machine:
ssh -L 7300:127.0.0.1:7300 user@appbox
# app's machine:
glass-mcp serve --http
```

Then point the client at `http://127.0.0.1:7300/`. The connection is encrypted by SSH; glass itself
does not own TLS. (Windows 10/11 ship the OpenSSH client, so the same flow works there.)

Register the resulting endpoint with your client as in [connect-an-agent.md](connect-an-agent.md#over-http).
