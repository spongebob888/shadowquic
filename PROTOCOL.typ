#set par(justify: true)
= Introduction
*Shadowquic* is 0-RTT QUIC based proxy with SNI camouflage.

Shadowquic doesn't provide any authentication layer.
Authentication is provided by JLS protocol.

= Command
Each proxy request is started with a command carried by a bistream.

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
  -  DOMAINNAME: `0x03`
  -  IP V6 address: `0x04`
-  ADDR       desired destination address
-  PORT desired destination port in network octet
    order

== TCP Proxy
TCP Connect command is supported to proxy forward TCP connection.
=== TCP Connect

TCP proxy task is directed followed by TCP Connect command like _socks5_

== UDP Proxy
```plain
+---------------+--------------+
|   SOCKSADDR   |  CONTEXT ID  |
+---------------+--------------+ 
|   Variable    |      2       |
+---------------+--------------+
```
UDP Associate command is carried by bistream called *control stream*. For each datagram received from local socket or remote socket a control header 

*control stream* doesn't send payload. The payload is carried by unistream or datagram extension chosen by user. 
Control stream *MUST* remain alive during udp association task.

UDP Associate command associates a remote socket to local socket. For each 
destination from a local socket the datagram will be asigned a `CONTEXT ID` which is in *one-one
corespondance* to 4 tuple (local udp ip:port, destination udp ip:port).

Each datagram will be prepended with a 2 bytes context ID. 

For each datagram from local socket or remote socket the `SOCKSADDR` and 
`CONTEXT ID` will be sent in the control stream.

=== Associate Over Stream
```plain
+---------------+--------------+--------------+
|  CONTEXT ID   |     LEN      |    PAYLOAD   |
+---------------+--------------+--------------+ 
|      2        |      2       |   Variable   |
+---------------+--------------+--------------+
```

If the datagram is carried by QUIC unistream, a 2 byte length tag is prepended to the payload. For every the same context id, the unistream could be reused.

=== Associate Over Datagram
```plain
+---------------+--------------+
|  CONTEXT ID   |    PAYLOAD   |
+---------------+--------------+ 
|      2        |   Variable   |
+---------------+--------------+
```
If datagrams are carried by QUIC datagram extension, the payload is sent directly without length field (only with `Context ID`).

