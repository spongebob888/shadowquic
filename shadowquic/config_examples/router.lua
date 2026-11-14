return function(ctx)
    -- Optional DNS preference for TCP and all datagrams in a UDP session.
    -- The request's preference overrides the outbound dns-strategy.
    -- ctx.dns_strategy = "prefer-ipv6"
    if ctx.dst_domain and string.match(ctx.dst_domain, "%.example$") then
        return "upstream"
    end
    return "direct"
end
