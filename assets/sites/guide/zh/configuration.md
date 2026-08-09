# 配置大白话

ShadowQUIC 的配置是一个 **YAML 文件**，结构就三段：

```yaml
inbound:    # 我怎么接流量
outbound:   # 流量往哪走
log-level:  # 日志多详细（可选）
```

不需要背，下面的表格把每个字段翻译成人话。

## 服务端配置（server.yaml）详解

```yaml
inbound:
  type: shadowquic        # 入站协议：别人怎么连进来
  bind-addr: "0.0.0.0:1443"   # 监听哪个端口（0.0.0.0 = 所有网卡）
  users:                  # 允许连接的账号密码列表
    - username: "admin"
      password: "hello"
  jls-upstream:           # ★ 伪装设置
    addr: "cloudflare.com:443"  # 伪装成访问这个域名（可换）
    rate-limit: 1000000   # 可选：转发限速（bps），默认不限
  server-name: "cloudflare.com"  # 可选：校验客户端用的服务器名
  alpn: ["h3"]            # TLS 应用层协议，保持默认即可
  congestion-control: bbr # 拥塞控制算法，bbr 在丢包网络更好
  zero-rtt: true          # 开启 0-RTT，连接更快
  initial-mtu: 1300       # 初始数据包大小，别乱改
  min-mtu: 1290           # 最小数据包大小，别乱改
outbound:
  type: direct            # 服务器收到请求后直连目标
  dns-strategy: prefer-ipv4  # DNS 解析偏好（见下表）
```

### 各字段白话解释

| 字段 | 大白话 |
|------|--------|
| `type` | 这段配置"是什么类型"。入站和出站都要填 |
| `bind-addr` | 监听的"地址:端口"。`0.0.0.0` 表示对外所有网卡 |
| `users` | 白名单。只有这里列出的账号密码能连进来 |
| `jls-upstream.addr` | 流量伪装成访问这个"域名:端口"。填真实的知名网站效果最好 |
| `jls-upstream.rate-limit` | 伪装转发流量时的速度上限（bit/s）。防止伪装服务器被滥用 |
| `server-name` | 可选：校验客户端用的服务器名。留空则从 `jls-upstream` 自动取 |
| `alpn` | TLS 握手时声明支持的协议。两端要有交集，默认 `["h3"]` 就行 |
| `congestion-control` | 网络差时用什么策略发数据。`bbr` 抗丢包，`cubic`/`new-reno` 是老牌 |
| `zero-rtt` | 第二次连接要不要免去握手等待。开着体验更好 |
| `gso` | 系统支持时批量发送数据，省 CPU。开着即可 |
| `mtu-discovery` | 自动探测网络能承受的最大包。UDP 网络不稳定时可关掉 |
| `min-mtu` | 最小数据包大小，不能大于 `initial-mtu`，别乱改 |
| `blackhole-detection` | 高丢包网络建议保持关闭，避免 MTU 被反复重置 |

> **JLS 伪装是怎么工作的？** 服务端把 `jls-upstream.addr` 指到的域名当作
> "替身"：客户端连上来时，TLS 握手的 SNI 字段就是那个域名，从外面看就像
> 在访问 `cloudflare.com`。`users` 里的用户名和密码同时充当 JLS 认证凭据，
> 客户端必须用服务端白名单里的账号才能通过握手。

## 客户端配置（client.yaml）详解

```yaml
inbound:
  type: socks             # 本地起一个 SOCKS5 代理
  bind-addr: "127.0.0.1:1089"   # 本地监听端口（软件都习惯用 1080/1089）
  users: []               # 可选：本机代理要不要密码（留空则不需要）
outbound:
  type: shadowquic        # 出站协议：把流量发给远程服务器
  addr: "1.2.3.4:1443"    # 服务器地址:端口（可填域名）
  username: "admin"       # 与 server.yaml 一致
  password: "hello"
  server-name: "cloudflare.com"  # ★ 必须与服务端 jls-upstream 域名一致
  alpn: ["h3"]
  zero-rtt: true
  congestion-control: bbr
  over-stream: false      # UDP 走数据报(false) 还是走流(true)
  keep-alive-interval: 0  # 保活间隔（毫秒），0=关闭
```

### 客户端字段补充

| 字段 | 大白话 |
|------|--------|
| `addr` | 远程服务器的"地址:端口"。填 IP 或域名都行 |
| `server-name` | 校验服务器身份的名字，**必须和服务端 `jls-upstream` 的域名一字不差** |
| `over-stream` | UDP 走哪种通道。`false`=数据报（低延迟、不重传）；`true`=走 QUIC 流（会重传，不适合大流量 UDP） |
| `keep-alive-interval` | 多久发一次心跳保活。单位毫秒，0 表示关掉（应小于 30000 毫秒空闲超时） |
| `bind-interface` | 强制出站数据包从指定网卡发出（如 `eth0`、`127.0.0.1`）。建议配合 sing-box/mihomo 等 TUN 代理用 |
| `protect-path` | Android 专用：指定 Unix socket 路径，让底层 socket 走 VPN 保护通道 |
| `cipher-suite-preference` | 可选：指定 TLS 1.3 密码套件偏好。不懂就留空，用默认顺序 |

## 入站类型：`inbound.type` 有哪些选择？

| type | 大白话 |
|------|--------|
| `socks` | 本地开一个 SOCKS5 代理端口，软件把流量丢给它 |
| `mixed` | 一个端口同时支持 SOCKS5 和 HTTP 代理，自动识别（需 `mixed` 特性） |
| `shadowquic` | 远程服务器专用：客户端用 `shadowquic` 协议连进来（JLS 伪装、免证书） |
| `sunnyquic` | 另一种 QUIC 协议（需真实证书、支持多路径），适合有证书的场景 |
| `tproxy` | Linux 透明代理：不需要改软件设置，路由表把流量导进来（仅 Linux、需 `tproxy` 特性） |

## 出站类型：`outbound.type` 有哪些选择？

| type | 大白话 |
|------|--------|
| `shadowquic` | 把流量加密后发给远程 shadowquic 服务器 |
| `sunnyquic` | 发给远程 sunnyquic 服务器 |
| `socks` | 转发给另一个 SOCKS5 代理（套娃） |
| `direct` | 直接访问目标网站（服务器常用） |

> `mixed`、`tproxy` 默认特性已开启，一般无需关心。用默认编译的版本即可。

## 拥塞控制算法

`congestion-control` 有几种选择：

| 值 | 大白话 |
|------|--------|
| `bbr` | 默认值。丢包时依然保持吞吐，网络差时首选 |
| `cubic` / `new-reno` | 传统算法，网络好时够用 |
| `brutal` | 激进抢占带宽（类似暴力加速）。**重要：** `brutal` 的值是一个对象而不是字符串，且它抢的是**上行**带宽；想限制下行得在对端（比如客户端对应服务器端）设置 |

`brutal` 写法示例：

```yaml
congestion-control:
  brutal:
    bandwidth: 10000000   # 上行带宽，单位 bps
```

## 一段话总结

- 配置文件 = `inbound`（接流量）+ `outbound`（送流量）
- **服务端**：`inbound` 用 `shadowquic`，`outbound` 用 `direct`，
  记得开防火墙 UDP 端口
- **客户端**：`inbound` 用 `socks`，`outbound` 用 `shadowquic`，
  记住 `server-name` 要和服务器伪装域名一致
- 拿不准的字段保持默认，先跑通再慢慢调

完整的字段列表（含默认值、必填项）见导航栏 **Configuration** 自动生成的手册。

## 下一步

看看 [常见问题 FAQ](faq.md) 或者回到[快速上手](quickstart.md)。
