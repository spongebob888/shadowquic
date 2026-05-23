#set par(justify: true)
= Introduction
*ShadowQUIC* is 0-RTT QUIC based proxy with SNI camouflage.

Shadowquic doesn't provide any authentication layer.
Authentication is provided by JLS protocol.

= Command
Each proxy request is started with a command carried by a bistream. Only client can initiate a command.

```plain
+-------+---------------+
|  CMD  |   SOCKSADDR   |
+-------+---------------+
|   1   |   Variable    |
+-------+---------------+
```
There are three types of command indicated by `CMD` field:
- `0x01` : TCP Connect
- `0x03` : UDP Association over datagram extension
  - The SOCKSADDR carries the binding address indicating which
    address and port client is listening on the remote. It is *NOT*
    the destination address.
- `0x04` : UDP Association over unistream


`SOCKSADDR` field is socks address format:
```plain
+---------+-----------+--------+
|  ATYP   |   ADDR    |  PORT  |
+---------+-----------+--------+
|    1    | Variable  |    2   |
+---------+-----------+--------+
```
- ATYP   address type of following address
  - IP V4 address: `0x01`
  - DOMAINNAME: `0x03`
  - IP V6 address: `0x04`
- ADDR       desired destination address
- PORT desired destination port in network octet
  order

== TCP Proxy
TCP Connect command is supported to proxy forward TCP connection.
=== TCP Connect

TCP proxy task is directed followed by TCP Connect command like _socks5_

== UDP Proxy
The UDP proxy scheme is greatly different from common protocol like TUIC/hysteria. The principle of design is to decrease datagram header size and reaches the *maximum MTU size*.

#import "@preview/cetz:0.5.0"

#figure(
  cetz.canvas(length: 1cm, {
    import cetz.draw: *

    let col-client = 2
    let col-proxy = 10
    let col-remote = 18

    // --- Participant boxes ---
    let box-w = 3
    let box-h = 0.8
    rect(
      (col-client - box-w / 2, 0.4),
      (col-client + box-w / 2, 0.4 + box-h),
      name: "client-box",
    )
    content("client-box", [*Client*])

    rect(
      (col-proxy - box-w / 2, 0.4),
      (col-proxy + box-w / 2, 0.4 + box-h),
      name: "proxy-box",
    )
    content("proxy-box", [*Proxy*])

    rect(
      (col-remote - box-w / 2, 0.4),
      (col-remote + box-w / 2, 0.4 + box-h),
      name: "remote-box",
    )
    content("remote-box", [*Remote*])

    // --- Lifelines ---
    let y-start = 0.0
    let y-end = -14.5
    set-style(stroke: (dash: "dashed", paint: gray))
    line((col-client, y-start), (col-client, y-end))
    line((col-proxy, y-start), (col-proxy, y-end))
    line((col-remote, y-start), (col-remote, y-end))
    set-style(stroke: black)

    // Helper to draw a message arrow with label
    let msg(y, from, to, label, style: "solid", color: black, lbl-anchor: "south") = {
      set-style(stroke: (paint: color, dash: style))
      line((from, y), (to, y), mark: (end: ">", fill: color))
      content(
        ((from + to) / 2, y + 0.3),
        anchor: lbl-anchor,
        padding: 0.1,
        label,
      )
      set-style(stroke: black)
    }

    // 1. UDP Associate command on bistream (control stream opens)
    let y = -1
    msg(y, col-client, col-proxy, text(size: 8pt)[CMD=0x03 | SOCKSADDR \ _(open control bistream)_], color: blue)

    // Note: control stream
    content(
      (col-client - 0, y - 0.5),
      anchor: "west",
      padding: 0.2,
      text(size: 7pt, fill: gray)[_control bistream stays open_],
    )

    // 2. Proxy binds remote UDP socket
    let y = -2.5
    msg(y, col-proxy, col-remote, text(size: 8pt)[Bind remote UDP socket], style: "dashed", color: gray)

    // 3. Client sends SOCKSADDR + CID for dest A on control stream
    let y = -4
    msg(y, col-client, col-proxy, text(size: 8pt)[SOCKSADDR(A) + CID=1 \ _(control stream)_], color: eastern)

    // 4. Client sends payload via datagram/unistream with CID
    let y = -5.5
    msg(y, col-client, col-proxy, text(size: 8pt)[CID=1 | Payload \ _(datagram / unistream)_], color: orange)

    // 5. Proxy forwards to remote dest A
    let y = -6.5
    msg(y, col-proxy, col-remote, text(size: 8pt)[UDP payload → dest A], color: orange)

    // 6. Remote responds
    let y = -7.8
    msg(y, col-remote, col-proxy, text(size: 8pt)[UDP reply from dest A], color: green)

    // 7. Proxy sends CID on control stream (server→client CID space)
    let y = -9
    msg(
      y,
      col-proxy,
      col-client,
      text(size: 8pt)[SOCKSADDR(A) + CID=1 \ _(control stream, server CID space)_],
      color: eastern,
    )

    // 8. Proxy sends payload back with CID
    let y = -10.2
    msg(y, col-proxy, col-client, text(size: 8pt)[CID=1 | Payload \ _(datagram / unistream)_], color: green)

    // 9. Another destination: client registers dest B
    let y = -11.5
    msg(y, col-client, col-proxy, text(size: 8pt)[SOCKSADDR(B) + CID=2 \ _(control stream)_], color: eastern)

    // 10. Client sends to dest B
    let y = -12.8
    msg(y, col-client, col-proxy, text(size: 8pt)[CID=2 | Payload \ _(datagram / unistream)_], color: orange)

    // 11. Proxy forwards to dest B
    let y = -13.3
    msg(y, col-proxy, col-remote, text(size: 8pt)[UDP payload → dest B], color: orange)

    // --- Close control stream note ---
    let y = -14.2
    content(
      ((col-client + col-proxy) / 2, y),
      padding: 0.2,
      text(size: 7pt, fill: red)[_control stream closed → UDP association terminated_],
    )
  }),
  caption: [UDP Association data transfer process],
)


The design has heavily considerred #link("https://www.rfc-editor.org/rfc/rfc9298.html")[RFC 9298]
```plain
+---------------+--------------+
|   SOCKSADDR   |  CONTEXT ID  |
+---------------+--------------+
|   Variable    |      2       |
+---------------+--------------+
```
UDP Associate command is carried by bistream called *control stream*.
For each datagram received from local socket or remote socket a control header
consists of `SOCKSADDR` and `CONTEXT ID` is sent.
This control header is sent at least once for each new `CONTEXT ID`.
The `CONTEXT ID` is used to identify the datagram stream of this destination
(even for receiving the returning packet of destination server).

When receving datagram from destination, the destination address must be the same as sending address.
If outgoing packet is domain address type, the receving packet must destination address must be the same.

For each connection, implementation must maintain two `CONTEXT ID` spaces.
One is for client to server direction. The other is for server to client direction.
These two id spaces are independent.
The header of both direction is sent in the *same* control bistream.

*control stream* doesn't send payload.
The payload is carried by unistream or datagram extension chosen by user.
Control stream *MUST* remain alive during udp association task.

If control stream is terminated, the udp association task *must* also be terminated.

UDP Associate command associates a remote socket to local socket. For each
destination from a local socket the datagram will be asigned a `CONTEXT ID` which is in *one-to-one
corespondance* to four tuple: source udp ip:port and destination udp ip:port.

Each datagram payload will be prepended with a 2 bytes context ID.

For each datagram from local socket or remote socket the `SOCKSADDR` and
`CONTEXT ID` pair will be sent in the control stream. And `SOCKSADDR` and
`CONTEXT ID` pair will be sent *at least once* for each new `CONTEXT ID`.

=== Associate Over Stream
```plain
+---------------+--------------+--------------+--------------+--------------+-----+
|  CONTEXT ID   |     LEN      |    PAYLOAD   |     LEN      |    PAYLOAD   | ... |
+---------------+--------------+--------------+--------------+--------------+-----+
|      2        |      2       |   Variable   |      2       |   Variable   | ... |
+---------------+--------------+--------------+--------------+--------------+-----+
```

If the datagram is carried by QUIC unistream, a 2 byte length tag is prepended to the payload. For the following datagram with the same context id, unistream could be reused,
and context id is not needed to be sent. Only LEN field and PAYLOAD will be sent.
Namely for each unistream, CONTEXT ID is sent only once right after this stream is opened,

=== Associate Over Datagram
```plain
+---------------+--------------+
|  CONTEXT ID   |    PAYLOAD   |
+---------------+--------------+
|      2        |   Variable   |
+---------------+--------------+
```
If datagrams are carried by QUIC datagram extension, the payload is sent directly without length field (only with `Context ID`).

= SunnyQUIC
*SunnyQUIC* is the twin protocol of *ShadowQUIC*. It is nearly the same as *ShadowQUIC*. The only
difference is that *SunnyQUIC* gives up JLS layer and provide a QUIC layer authentication.
The underlying connection is native QUIC connection.

== Authentication
SunnyQUIC adds a new _authentication_ command. The command is carried by the bistream.

```plain
+-------+---------------+
|  CMD  |   AUTH_HASH   |
+-------+---------------+
|   1   |       64      |
+-------+---------------+
```
The `CMD` field is `0x5` for authentication command, The `AUTH_HASH` field is truncated 64byte hash:
`SHA256(username:password)[0..64]`

For client, the authentication command can be issued in parallel with other proxy commands.

For server, it should block any commands until authentication is finished. If authentication fails, server should terminate the connection.
