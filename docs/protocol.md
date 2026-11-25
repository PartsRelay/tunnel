# Tunnel protocol, version 1

This is what the tunnel speaks to the server it connects to (`connectors` at Parts Relay). The tunnel is the client of the connection and the server of the streams: it dials out once, then the server opens one stream for every TCP connection it wants to reach in the private network.

```
connectors  <== WebSocket over TLS, port 443 ==  tunnel  -- TCP -->  management system
  yamux client, opens streams                    yamux server, dials the allowed targets
```

## 1. Connection

The tunnel opens a WebSocket to its configured `server`, by default `wss://connectors.partsrelay.com/v1/tunnel`, through an HTTP proxy with `CONNECT` when one is set.

Request headers:

| Header                   | Value                                             |
| ------------------------ | ------------------------------------------------- |
| `Authorization`          | `Bearer <token>`, the token of the tunnel         |
| `User-Agent`             | `partsrelay-tunnel/<version> (<target triple>)`   |
| `Sec-WebSocket-Protocol` | `partsrelay-tunnel.v1`                            |

The server answers `101` and must echo `Sec-WebSocket-Protocol: partsrelay-tunnel.v1`: the tunnel drops a connection without it. A later version of the protocol gets a new subprotocol name.

Any other answer is a refusal:

| Status           | Meaning for the tunnel                                                    |
| ---------------- | ------------------------------------------------------------------------- |
| `401`, `403`     | The token is not accepted. It waits up to 1 minute, then up to 15 minutes |
| `429`, `503`     | Not now. It waits `Retry-After` seconds if present, at most 15 minutes and never less than its usual backoff |
| anything else    | It tries again with its usual backoff. `Retry-After` is not read          |

A proxy that answers the `CONNECT` with `407` is handled like `401`.

Redirects are not followed and no WebSocket extension is used. The tunnel accepts `ws://` only for `localhost`, `127.0.0.0/8` and `::1`, and never sends such a connection through a proxy.

### Probe

`partsrelay-tunnel check` tests the token with the same handshake plus one header:

| Header                    | Value |
| ------------------------- | ----- |
| `Partsrelay-Tunnel-Probe` | `1`   |

The server checks the token and answers `101` or a refusal as usual, but a probe is not a session: it must not replace the session of the token, and no stream is opened on it. The tunnel closes a probe at once with a WebSocket Close.

The usual backoff starts at 1 second and doubles up to 60 seconds, each wait drawn from the upper half of its step. It starts again from 1 second after a session that lasted 60 seconds.

## 2. Messages

Only binary messages carry data. Their boundaries mean nothing: the payloads of the binary messages, in order, form one byte stream in each direction, and that byte stream carries yamux.

- A receiver accepts messages up to 1 MiB. A sender should keep them at or below 64 KiB.
- A text message is a protocol error and ends the connection.
- The tunnel sends a WebSocket ping every 20 seconds and drops the connection when no message of any kind arrives for 60 seconds. The server answers the pings, as WebSocket libraries do by themselves, and should apply the same rule.

Close codes the tunnel understands:

| Code   | Meaning                                                                           |
| ------ | --------------------------------------------------------------------------------- |
| `4001` | Replaced by another tunnel with the same token. It waits 5 minutes before dialing |
| `1008` | The token is no longer accepted. Handled like `401`                               |
| other  | It dials again with its usual backoff                                             |

The server should keep one session per token and close the older one with `4001`, so that two installs with the same token do not fight. The tunnel reads the close code also when a yamux GoAway came first.

## 3. Multiplexing

The byte stream carries [yamux](https://github.com/hashicorp/yamux/blob/master/spec.md), version 0.

- The tunnel is the yamux server and never opens a stream. `connectors` is the yamux client and opens them, with odd stream ids.
- The initial window of a stream is 256 KiB. The tunnel keeps at most 512 KiB times `max_streams` of unread stream data, 32 MiB by default. A frame larger than the window left, or than 1 MiB, ends the session.
- A WINDOW_UPDATE and a reset are honored also after the FIN of their sender.
- The tunnel sends a yamux ping about every 10 seconds, to measure the round trip, and the server acknowledges it with the same value. An acknowledgement nobody asked for ends the session. The tunnel acknowledges the pings of the server. Whether the connection is alive is decided by the WebSocket, see section 2.
- The tunnel relays at most `max_streams` streams at once, 64 by default, and refuses the ones opened beyond that with `too_many_streams`. A server that holds open twice that many ends the session.
- Before a planned stop the tunnel refuses new streams with `shutting_down` and waits up to 30 seconds for the open ones. It then sends what they still have queued, a yamux GoAway and a WebSocket Close.

## 4. A stream

A stream starts with one request frame from its opener and one response frame from the tunnel. A frame is a length and that many bytes of UTF-8 JSON:

```
+----------------------+---------------------+
| length: u16, big end | JSON, length bytes  |
+----------------------+---------------------+
```

The length is between 1 and 4096.

Request:

```json
{ "target": "erp.local:8080" }
```

`target` is `host:port`, with an IPv6 address in brackets like `[fe80::1]:5432`. Unknown fields are ignored. The tunnel reads the request for at most 10 seconds.

Response, accepted:

```json
{ "ok": true }
```

Response, refused, after which the tunnel closes the stream:

```json
{ "ok": false, "error": "not_allowed", "message": "erp.local:8081 is not allowed" }
```

| `error`           | When                                                                      |
| ----------------- | ------------------------------------------------------------------------- |
| `bad_request`     | No request in 10 seconds, a bad length, bad JSON or a bad `target`        |
| `not_allowed`     | The target is not in the `allow` list of the tunnel. Nothing was dialed   |
| `resolve_failed`  | The name of the target has no address                                     |
| `connect_refused` | The target refused the connection                                         |
| `connect_timeout` | The target did not answer in 10 seconds                                   |
| `connect_failed`  | Any other failure to connect                                              |
| `too_many_streams` | The tunnel already relays `max_streams` streams. Open it again later     |
| `shutting_down`   | The tunnel is stopping or restarting after an update. Open it again later |

`message` is for people and logs. Programs read `error`.

`too_many_streams` and `shutting_down` are answered at once, without reading the request. When the name of a target has several addresses, each gets an equal share of the 10 seconds.

After `{ "ok": true }` the stream is the TCP connection: every byte written to one side is written to the other, in both directions, with nothing added. The opener may send payload right behind its request, without waiting for the response.

- When either side finishes writing (TCP FIN, or a yamux FIN), the other side sees the end of that direction and may keep writing in the other one.
- A yamux reset closes the TCP connection. A TCP error becomes a yamux reset while the opener is still writing. After the FIN of the opener the tunnel can only end the stream with its own FIN, so an opener that half closes tells a short answer by the framing of its own protocol.
- An opener that finished writing must keep reading, or reset the stream. When the tunnel cannot write to such a stream for 60 seconds, it ends it and closes the TCP connection.
- There is no idle timeout on a stream: a database connection may stay quiet for hours.

## 5. What the allow list compares

The tunnel dials a target only when its `allow` list holds exactly that `host:port`.

- A name is compared without its case and without a trailing dot. `ERP.Local.:8080` is `erp.local:8080`.
- Addresses are compared as addresses, so `[FE80:0:0:0:0:0:0:1]:5432` is `[fe80::1]:5432`.
- A name never allows its address, and an address never allows a name that resolves to it.
- There are no wildcards and no port ranges.

So `connectors` must ask for a target exactly as the administrator of the tunnel wrote it.
