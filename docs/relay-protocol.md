# Relay protocol

How the Stonewave daemon (`stonewaved`, started as `stonewave daemon`) on your server talks to Wave. The daemon only connects **outbound** (HTTPS, port 443). Nothing listens for Wave on your server.

What the relay can do with this line: carry bytes to the **agent SSH on `127.0.0.1:2222`** of your server, and cut the line. What the agent may then do is decided by that SSH with certificates, not by Wave. The relay holds no key that is trusted on your server. The daemon cannot run a command; it opens a TCP connection to its fixed local address and copies bytes.

## The server key

`stonewave connect` makes an Ed25519 key on the server (`/var/lib/stonewave/server.key`, mode 600, owner `stonewave`). The private half never leaves the server. The public half is registered with Wave when the server enrols.

## Enrolment: `POST /v1/relay/enrol`

`Authorization: Bearer <one-time token of the sign-in>` (valid 15 minutes). JSON body `{public_key, proof, host, os, version}`:

- `public_key`: the 32 raw bytes of the Ed25519 public key, base64url without padding.
- `proof`: an Ed25519 signature (base64url) over `"sw-enrol1\n" + sha256hex(token) + "\n" + public_key`. Only the holder of the key can enrol it, and only for this token.

The answer is `{server_ref, server_id, name, status, company}`. `server_ref` (`<company id>.<server id>`) is not a secret; it is saved in `/var/lib/stonewave/server.json` with the relay address.

## Signed requests

Every other request carries:

| Header | Value |
|---|---|
| `Authorization` | `Stonewave-Server <server_ref>` |
| `X-SW-Time` | unix time in milliseconds (at most 60 s off) |
| `X-SW-Nonce` | 16 random bytes, base64url, used once |
| `X-SW-Sig` | Ed25519 signature, base64url |
| `X-SW-Session` | the session from the stream's `hello` (only for `/up`) |

The signature is over these lines joined by `\n`:

```
SW1
<METHOD>
<path and query as sent>
<X-SW-Time>
<X-SW-Nonce>
<sha256 hex of the body; of the empty string for GET>
<server_ref>
<session, or - for the stream request>
```

A signature for one request is worthless for any other. `src/relay.rs` has a test vector (`signs_exactly_like_the_relay_expects`) that the relay's own tests check too.

## `GET /v1/relay/stream` (Wave to server)

A response that does not end: one JSON object per line. One stream per server; a new one replaces the old. It ends after at most 10 minutes (`bye`) and the daemon reconnects with a growing pause (1 s up to 30 s). The daemon treats 90 s of silence as a dead line.

| Frame | Meaning |
|---|---|
| `{"t":"hello","session",...,"heartbeat_s"}` | first line; the session goes into `X-SW-Session` |
| `{"t":"ping"}` | every 20 s |
| `{"t":"open","ch"}` | connect a channel to the agent SSH (at most 4 at a time) |
| `{"t":"data","ch","d"}` | bytes for the SSH (`d` base64url, at most 16 KiB before encoding) |
| `{"t":"close","ch"}` | end the channel |
| `{"t":"bye","reason"}` | `revoked`: Wave cut this server off for good. The daemon closes the line, waits 20 s and asks again on a new connection; if Wave says it a second time (and no healthy line came up in between), it exits with status 78 and systemd does not restart it. Other reasons: reconnect |

`open` carries a channel number and nothing else. Frame types this version does not know are ignored.

**Known limit.** What Wave sends down is protected by TLS only; the frames carry no signature of Wave's own and the daemon does not pin the relay's certificate. Someone who sits inside the TLS connection (an inspecting proxy that the server trusts) could send frames, and could make the daemon stop by answering `revoked` every time. The two-step `revoked` above stops a single injected frame, not a proxy that keeps answering. What an `open` or `data` frame can reach is still limited by the agent SSH, which wants a certificate the admin's device signed. Signed down-frames (or a pinned relay key) need a key of Wave's own in the contract; until then this stays a known limit.

## `POST /v1/relay/up` (server to Wave)

NDJSON body, at most 256 KiB and 64 frames. Frames are sent in batches (frames that come close together share one request):

- `{"t":"opened","ch"}`, `{"t":"open_failed","ch"}`, `{"t":"close","ch"}`
- `{"t":"data","ch","d"}`
- `{"t":"health","v":{ts, version, checks[{id, level, detail}], metrics{...}, units{...}}}` once a minute: the `stonewave doctor` checks and a few fixed numbers (disk, memory, load, uptime, pending updates, whether the two SSH services run). Nothing else is ever sent: no file names, no process list, no logs.

## Sizes and limits

The daemon posts with the system's `curl` (one request per batch). Config goes to curl on stdin, so signatures never show in a process list. Plain `http://` addresses are accepted only for the local machine (`127.0.0.1`, `localhost`, `::1`), for tests.

## On the server

`stonewave status` shows the line (`/var/lib/stonewave/tunnel.json`), the agent SSH and the last health report. `sudo stonewave revoke` stops the daemon and the agent SSH, empties the trust list and locks the agent user, also without internet. `sudo stonewave setup` opens it again.
