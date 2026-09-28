# 常见问题 FAQ

## `shadowquic` 和 `sunnyquic` 有什么区别？

同一个程序里内置了两种协议，配置文件的 `type` 字段决定用哪个：

| | `shadowquic` | `sunnyquic` |
|---|---|---|
| 伪装 | **SNI 伪装**，流量像访问知名网站，无需证书 | 真实 TLS 证书 |
| 证书 | 不需要自己申请 | 服务端需要配置 `cert-path`/`key-path` |
| 多路径 | 单路径 | 支持多路径（`max-path-num`、`extra-paths`），多网卡/双栈可聚合 |
| 适用 | 普通用户、怕被墙的场景（推荐） | 有证书、需要多路径/高可靠网络的场景 |

它们共享同一套 QUIC 传输和 UDP 会话逻辑，只是"开门方式"不同。

## 连不上服务器 / 握手超时？

最常是这三个原因，逐个排查：

1. **防火墙没放行 UDP 端口**。ShadowQUIC 走的是 UDP 不是 TCP。
   到服务器控制台的安全组 / 防火墙，把 `bind-addr` 的端口对 **UDP**
   放行（TCP 不放行也没关系）。
2. **`server-name` 和服务器不一致**。客户端 `server-name` 必须和
   服务端 `jls-upstream` 里的域名**一字不差**（包括大小写、是否带端口）。
3. **`alpn` 没有交集**。两端 `alpn` 至少要有一个共同的协议，默认
   `["h3"]` 别乱改。

## 能连上但访问很慢 / 丢包严重？

- 服务端和客户端都把 `congestion-control` 设为 `bbr`（抗丢包最好）。
- 网络丢包率高时，把 `mtu-discovery` 设为 `false` 并手动调大
  `initial-mtu`（比如 `1400`）。
- 高丢包网络建议关掉 `blackhole-detection`（默认就是关的），
  避免 MTU 被反复重置。

## UDP（游戏/语音）不通？

- 确认客户端 `over-stream: false`（UDP 走数据报模式，延迟最低）。
- ShadowQUIC 支持 **Full Cone**，打洞能力强；但如果你的路由器/运营商
  NAT 太严格，仍然可能受影响。
- 想代理 HTTP/3 流量时，保持 `over-stream: false` 并且关掉
  `blackhole-detection`，避免破坏 HTTP/3 的 MTU 探测。

## 服务器需要域名和证书吗？

不需要！这是 ShadowQUIC 的招牌特性：

- `jls-upstream.addr` 填一个**别人**的知名网站（如 `cloudflare.com:443`）。
- 你的服务器会把这个域名当作"伪装身份"，客户端用 `server-name` 来认它。
- 全程不需要自己申请 TLS 证书。

> 注意：伪装用的域名必须是**真实存在且可用**的，否则会被探测出来。

## `admin` 前缀的账号有什么用？

用户名以 `admin` 开头的账号（如 `admin`、`admin_bob`、`admin123`）拥有
**管理权限**，可以通过 `shadowquic api` 子命令远程管理服务器：

```bash
shadowquic api list-users                        # 列出所有用户
shadowquic api add-user alice alice-pass         # 新增/更新用户
shadowquic api remove-user bob                   # 删除用户（同时断开其连接）
shadowquic api get-stats alice                   # 查看某用户流量统计
shadowquic api get-stats                         # 查看所有用户流量统计
shadowquic api kill-conn alice                   # 断开某用户所有在线连接
shadowquic api clear-stats alice                 # 清零某用户流量统计
shadowquic api clear-stats                       # 清零所有用户流量统计
```

普通账号只能代理流量，调用这些管理接口会返回 `PermissionDenied`。

`api` 子命令使用配置文件里的 `outbound` 来连服务器，所以执行前要确保
outbound 是 `shadowquic` 或 `sunnyquic`（`socks`/`direct` 不支持）。

## 一个端口能服务多个用户吗？

能。服务端 `users` 列表里可以写任意多个账号密码，大家共用同一个
`bind-addr` 端口。每个用户还能独立看流量统计（`api get-stats <名字>`）。

## 手机（Android/iOS）能用吗？

能用，但核心程序跑在服务端 + 一台电脑/路由器上：

- Android：配合 [nekobox](https://github.com/MatsuriDayo/nekobox)、v2rayN
  等客户端使用，见仓库 `document/clients/windows.md`。
- 也可以用支持 ShadowQUIC 协议的现成客户端（如
  [Clash-rs](https://github.com/Watfaq/clash-rs)、
  [husi](https://github.com/xchacha20-poly1305/husi)、
  [QuicProxy](https://github.com/RealBikiniBottom/QuicProxy)、mihomo）。

## 客户端一定要用官方程序吗？

不一定。协议是公开的（仓库里有 `PROTOCOL` 文档），上面列出的第三方
客户端都实现了 ShadowQUIC / SunnyQUIC 协议。跨实现互通性测试见官方
Interop 页面。

## 能不能多台服务器 / 做负载均衡？

ShadowQUIC 本身是单服务器模型。想要多节点、自动切换，建议搭配
Clash-rs / mihomo 这类上层客户端，它们把多个 ShadowQUIC 节点聚合成
策略组。

## 遇到别的报错？

- 启动报错：先看日志。把 `log-level` 临时改成 `trace` 会输出最详细的信息。
- 程序崩溃 / 行为诡异：去仓库 [Issues](https://github.com/spongebob888/shadowquic/issues)
  搜一下，八成有人遇到过；没有就带上日志提一个 Issue。

## 相关链接

- 完整配置字段：导航栏 **Configuration** 章节（自动生成，和代码同步）
- 用户管理 API：导航栏 **API** 章节
- 协议细节：导航栏 **Protocol** 章节（极客向）
