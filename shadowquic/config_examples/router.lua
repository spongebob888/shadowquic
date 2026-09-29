return function(ctx)
    if ctx.dst_domain and string.match(ctx.dst_domain, "%.example$") then
        return "upstream"
    end
    return "direct"
end
