= Introduction
Shadowquic is 0-RTT QUIC based proxy with SNI camouflage.

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
- `0x03` : UDP Association over datagram
- `0x04` : UDP Association over stream


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

== TCP Connect
TCP proxy task is directed followed by TCP Proxy command

== UDP Association over stream

== UDP Association over datagram

