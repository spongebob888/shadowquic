# API 子命令

`api` 子命令通过配置文件的 `outbound` 部分调用 SQuic 用户管理控制面 API。
outbound 必须是 `shadowquic` 或 `sunnyquic`；`socks` 和 `direct` 出站没有
实现这些 API。

配置中的 outbound 用户名必须以 `admin` 开头，例如 `admin`、`admin_bob`、
`admin123`。其他用户仍然可以代理流量，但调用 API 会返回 `PermissionDenied`。

默认情况下，CLI 读取当前目录下的 `config.yaml`。使用 `-c` 或 `--config`
指定其他客户端配置。该标志是全局的，可以放在 `api` 之前或之后。

```sh
shadowquic api list-users
shadowquic -c shadowquic/config_examples/client.yaml api list-users
shadowquic api --config shadowquic/config_examples/client.yaml list-users
```

从源码运行时，子命令需放在 `--` 之后：

```sh
cargo run -p shadowquic -- api list-users
```

## 子命令

### `list-users`

列出远程服务器上配置的所有用户名。

```sh
shadowquic api list-users
```

命令每行输出一个用户名：

```text
admin
alice
bob
```

### `add-user <username> <password>`

向远程服务器添加新用户。

```sh
shadowquic api add-user alice alice-pass
```

如果用户名已存在，服务器会更新该用户的密码。成功后打印：

```text
user added: alice
```

### `remove-user <username>`

从远程服务器移除用户。

```sh
shadowquic api remove-user alice
```

移除用户同时会关闭该用户的活跃连接。成功后打印：

```text
user removed: alice
```

如果用户不存在，命令会以 `NotFound` 失败。

### `get-stats [username]`

获取一个用户的流量与连接统计；省略 `username` 时获取所有已配置用户的统计。

```sh
shadowquic api get-stats alice
```

输出字段：

```text
username: alice
conn_num: 1
tcp_conns: 1
tcp_sent: 4096
tcp_recv: 4096
udp_conns: 1
udp_sent: 777
udp_recv: 777
```

`conn_num` 是该用户在线 QUIC 连接数。`tcp_conns` 和 `udp_conns` 是活跃的代
理 TCP 流与 UDP 关联数。`tcp_sent` 和 `udp_sent` 统计从服务器发回客户端的字
节数。`tcp_recv` 和 `udp_recv` 统计服务器从客户端收到的字节数。

流量统计依赖 `statistics` 特性，默认开启。在没有原生 64 位原子操作的目标
平台（如 32 位 MIPS）上，计数器用 `portable-atomic` 模拟，仍然可用。

省略用户名获取所有用户的统计：

```sh
shadowquic api get-stats
```

命令为每个用户打印一个 `get-stats` 风格的块，块之间用空行分隔：

```text
username: admin
conn_num: 1
tcp_conns: 0
tcp_sent: 0
tcp_recv: 0
udp_conns: 0
udp_sent: 0
udp_recv: 0

username: alice
conn_num: 1
tcp_conns: 1
tcp_sent: 4096
tcp_recv: 4096
udp_conns: 1
udp_sent: 777
udp_recv: 777
```

### `clear-stats [username]`

清零一个用户（省略用户名时清零所有用户）的累计流量字节计数器
（`tcp_sent`、`tcp_recv`、`udp_sent`、`udp_recv`）。

```sh
shadowquic api clear-stats alice
```

成功后打印：

```text
user stats cleared: alice
```

省略用户名会清零所有已配置用户：

```sh
shadowquic api clear-stats
```

```text
all user stats cleared
```

只有四个字节计数器被重置。活跃连接计数器（`conn_num`、`tcp_conns`、
`udp_conns`）不会被触碰。重置后活跃连接从零开始继续计数。

该重置是尽力而为的：四个计数器独立清零，而不是一次原子快照，因此在活跃
流量下并发的 `get-stats` 可能会短暂观察到清零与未清零计数器混合的情况。

只有管理员用户（`admin`、`admin_bob`、……）能运行此命令，其他用户会得到
`PermissionDenied`。清零不存在的用户会以 `NotFound` 失败。

客户端和服务端都必须运行支持 `clear-stats` 的版本；旧服务器不识别该请求。

### `kill-conn <username>`

关闭用户的所有在线 QUIC 连接。

```sh
shadowquic api kill-conn alice
```

成功后打印：

```text
user connections killed: alice
```

用户仍然保留在配置中，可以用相同密码重新连接。如果想同时删除用户，请使用
`remove-user`。

## 常见错误

`PermissionDenied` 表示客户端配置中的 outbound 用户名不是以 `admin` 开头。

`NotFound` 表示目标用户名在服务器上不存在。

`NotAvailable` 表示连接的服务器或协议实现不支持所请求的 API。

`api requires a shadowquic or sunnyquic outbound config` 表示所选配置文件
的 outbound 是 `socks` 或 `direct`。请使用 outbound 连接到 ShadowQuic 或
SunnyQuic 服务器的客户端配置。
