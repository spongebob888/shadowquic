# DNS Design of Shadowquic

The dns feature is an optional feature controlled by "dns-server" feature flag. It provides 
an dns server support listening on a certain address.

## Architecture
The DNS Server implements inbound trait. It listens on an address and acception returns a ProxyRequest which is
the underlying traffic to make dns request. The proxyrequest can be routed and  handled by an outbound 
to perform the real traffic.

It conatains a resolver, the resolver implements DnsService trait and outbound trait

It should also implement DnsService trait which provide async resolve method for resolving a domain name and exchange method to return raw bytes, it also provides reverse_lookip for looking up domain for an ip address

It should also implement outbound trait which accepts a hijacked udp dns traffic

## Features
It supports following dns type:
- dns over udp
- dns over tcp
- dns over tls, the tls use the rustls lib
- fakeip dns. At most one instance and if it exists, perform fake ip mapping in tproxy. The fakeip should not exist in router stage.
- system dns, which use tokio lookup function to perform dns.

Each type corresponding to an inbound type.

## Tips
- Use simple dns lib to pack/unpack a dns request
- A global dns cache should be implemented. It also should support reverse lookup
- expose a reverse_lookup_cache method to route script
- expose a lookup_cache method to route script
- each outbound can choose a addr resolver to resolve its own addr. 
- Create a threadsafe ResolverManager to manage all resolvers and dns cache. Each router should has a copy for lookup dns
