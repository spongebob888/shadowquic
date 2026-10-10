return function(ctx)
  -- The database download inbounds are ordinary TCP connections. Route them
  -- directly so the databases can bootstrap themselves.
  if ctx.inbound_tag == "geoip-db" or ctx.inbound_tag == "geosite-db" then
    return "proxy"
  end

  -- The DNS inbounds use UDP or TCP connections. Route them directly so the DNS can bootstrap itself.
  if ctx.inbound_tag == "tls-dns" or ctx.inbound_tag == "lan-dns" then
    return "direct"
  end
  
  -- Hijack all DNS queries from the tproxy-inbound and route them to the hijack-dns inbound.
  -- The hijack-dns inbound will parse the DNS queries and fill dns_query in the context for the router to use.
  if ctx.inbound_tag == "tproxy-in" and ctx.dst_port == 53 then
    return "hijack-dns"
  end

  if ctx.inbound_tag == "hijack-dns" then
    -- routing local dns query like openwrt.lan to lan-dns.
    if #ctx.dns_query > 0 and ctx.dns_query[1].name:sub(-4) == ".lan" then
      return "lan-dns"
    end
    if #ctx.dns_query > 0 and find_domain("geosite-db", "geolocation-cn", ctx.dns_query[1].name) then
      return "tls-dns"
    end
    return "fake-dns"
  end

  if ctx.src_ip_v4 then
    -- reverse lookup is supported in the router
    -- It is useful for access control of devices in the LAN.
    -- It may be slow
    -- local src_host = reverse_lookup("lan-dns",  ctx.src_ip_v4)[1]:lower()
    -- if src_host and src_host == "xiaomi 14.lan" then
    --   return "direct"
    -- end
  end

  -- CN domain names.
  if ctx.dst_domain then
    local ok, cn = pcall(find_domain, "geosite-db", "geolocation-cn", ctx.dst_domain)
    if ok and cn then return "direct" end
  end

  -- CN IP ranges from the country database (the primary path for tproxy).
  if ctx.dst_ip_v4 then
    local ok, cn = pcall(find_ip_v4, "geoip-db", "CN", ctx.dst_ip_v4)
    if ok and cn then return "direct" end
  end
  if ctx.dst_ip_v6 then
    local ok, cn = pcall(find_ip_v6, "geoip-db", "CN", ctx.dst_ip_v6)
    if ok and cn then return "direct" end
  end

  -- Everything else (non-CN public traffic) goes through shadowquic.
  return "proxy"
end

