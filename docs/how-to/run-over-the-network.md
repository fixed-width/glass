# Run Glass over the network

Run Glass on the machine with the target application or device. When the agent connects from
another machine, serve Streamable HTTP and register the server URL in the MCP client.

| Situation | Transport | How |
|---|---|---|
| Client launches a local server | stdio | Register the binary; the client spawns it. |
| Local macOS menu-bar or Windows tray app | HTTP | Register the resident app's endpoint and keep it running. |
| Agent and app on different machines | network (HTTP) | Run `glass-mcp serve --http --addr …` on the app's machine; point the client at the URL with a bearer token. |

The network transport is behind the default-on `network` cargo feature; a `--no-default-features`
build is stdio-only.

## With a bearer token (non-loopback bind)

Generate a token, then serve with it:

```bash
mkdir -p ~/.glass
glass-mcp gen-token --out ~/.glass/token                  # cross-platform CSPRNG token
glass-mcp serve --http --tool-profile lean --addr 0.0.0.0:7300 --token-file ~/.glass/token
```

The client supplies the token as an `Authorization: Bearer <token>` header (check your client's docs
for its bearer-token / headers field). Binding a non-loopback address **without** a token is refused
at startup — fail-closed. You can pass the token via `GLASS_TOKEN` instead of `--token-file`.

> **Token-file permissions on Windows.** On Linux, Glass forces the token file to owner-only
> (`0600`). On Windows the file inherits the **permissions of the folder** you write it into; Glass
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

Then point the client at `http://127.0.0.1:7300/`. The connection is encrypted by SSH; Glass itself
does not own TLS. (Windows 10/11 ship the OpenSSH client, so the same flow works there.)

Register the resulting endpoint with your client as in [connect-an-agent.md](connect-an-agent.md#over-http).
