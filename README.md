Network Quality Exporter
========================

Exporter for Prometheus that keeps an eye on network quality. Put something like this in `/etc/network-quality-exporter.toml`:

```toml
# ping this host every 2.5 seconds
[[ping]]
name = "fubuki"
host = "dark.red"
interval = 2.5

[[ping]]
name = "cloudflare"
host = "1.1.1.1"
# interval = 1 by default

[[ping]]
name = "router"
host = "192.168.0.1"

# send "fubuki\n" to this host every 10 seconds
# waits for any response (content doesn't matter)
[[udpecho]]
name = "fubuki"
host = "dark.red"
payload = "fubuki\n"
interval = 10 # default is 30
```

Logging can be controlled with `RUST_LOG=warn` (error, warn, info, debug, trace)
