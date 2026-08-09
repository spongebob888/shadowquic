# 5 分钟快速上手

目标：**一台服务器 + 一台本地电脑**，让本地流量通过隧道出去。

## 第 1 步：下载程序

从 [GitHub Releases](https://github.com/spongebob888/shadowquic/releases)
下载对应你系统的 `shadowquic` 可执行文件（一个文件，解压即用）。

Linux 也可以用官方一键安装脚本（会自动装到系统并创建 systemd 服务）：

```bash
curl -L https://raw.githubusercontent.com/spongebob888/shadowquic/main/scripts/linux_install.sh | bash
```

> 想从源码编译？看仓库根目录 README 的 Build 章节。

## 第 2 步：配置服务端

在**远程服务器**上创建一个 `server.yaml`：

```yaml
inbound:
  type: shadowquic
  bind-addr: "0.0.0.0:1443"        # 监听所有网卡的 1443 端口
  users:
    - username: "admin"            # 客户端要用的账号
      password: "hello"
  jls-upstream:
    addr: "cloudflare.com:443"     # 伪装成访问 cloudflare.com（可换任何域名）
  alpn: ["h3"]
outbound:
  type: direct                     # 服务器直连访问目标网站
log-level: "info"
```

启动：

```bash
./shadowquic -c server.yaml
```

> 注意：`bind-addr` 的端口（1443）需要在防火墙/安全组里**放行 UDP**。

## 第 3 步：配置客户端

在**本地电脑**上创建 `client.yaml`：

```yaml
inbound:
  type: socks
  bind-addr: "127.0.0.1:1089"      # 本地 SOCKS5 代理端口
outbound:
  type: shadowquic
  addr: "你的服务器IP:1443"          # 改成你服务器的地址
  username: "admin"                 # 和 server.yaml 一致
  password: "hello"
  server-name: "cloudflare.com"     # 必须和服务端 jls-upstream 域名一致
  alpn: ["h3"]
log-level: "info"
```

启动：

```bash
./shadowquic -c client.yaml
```

## 第 4 步：验证通了没有

终端里执行（macOS/Linux 自带 `curl`）：

```bash
curl --socks5-hostname 127.0.0.1:1089 https://example.com
```

能看到网页内容就说明**成功了**。也可以直接测出口 IP：

```bash
curl --socks5-hostname 127.0.0.1:1089 https://ipinfo.io/ip
```

看到的是**你服务器的 IP**，而不是你本地的 IP，就说明流量确实走了隧道。

## 第 5 步：让浏览器/系统使用它

- **浏览器**：安装 SwitchyOmega 之类的扩展，指向 `SOCKS5 127.0.0.1:1089`。
- **系统**：把系统代理设为 `127.0.0.1:1089`（SOCKS5）。
- **手机 / Clash 客户端**：ShadowQUIC 有现成的协议支持（见 README 中的
  [Clash-rs](https://github.com/Watfaq/clash-rs) 等其他客户端）。

## 出问题了吗？

先看 FAQ：[常见问题](faq.md)。常见坑是防火墙没放行 UDP 端口、
`server-name` 与 `jls-upstream` 域名不一致。
