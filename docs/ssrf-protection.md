# SSRF Protection

The `web.fetch` tool includes comprehensive Server-Side Request Forgery (SSRF) protection to prevent the AI agent from accessing internal infrastructure, cloud metadata endpoints, or private networks.

## Implementation

SSRF protection is implemented in `src/agents/tools/web_fetch.rs` via two functions:

- `is_ssrf_target(url)` — URL-level checks (scheme, hostname patterns)
- `is_private_ip(ip)` — IP-level checks (IPv4 and IPv6 private ranges)

## Blocked Categories

### Scheme Restrictions

Only `http://` and `https://` URLs are allowed. All other schemes (file, ftp, gopher, etc.) are blocked.

### Hostname Blocking

| Pattern | Example | Reason |
|---------|---------|--------|
| `localhost` | `http://localhost:8080` | Loopback |
| `127.0.0.1` | `http://127.0.0.1/admin` | IPv4 loopback |
| `::1` / `[::1]` | `http://[::1]:3000` | IPv6 loopback |
| `*.localhost` | `http://app.localhost` | Localhost suffix (RFC 6761) |
| `*.internal` | `http://service.internal` | Internal DNS |
| `*.local` | `http://printer.local` | mDNS/Bonjour |
| `*.svc.cluster.local` | `http://api.default.svc.cluster.local` | Kubernetes services |
| `metadata.google.internal` | `http://metadata.google.internal` | GCP metadata |
| `169.254.169.254` | `http://169.254.169.254/latest/meta-data/` | Cloud metadata (AWS, GCP, Azure) |

### IPv4 Private Ranges

| Range | CIDR | Description |
|-------|------|-------------|
| `10.0.0.0 - 10.255.255.255` | `10.0.0.0/8` | RFC 1918 private |
| `172.16.0.0 - 172.31.255.255` | `172.16.0.0/12` | RFC 1918 private |
| `192.168.0.0 - 192.168.255.255` | `192.168.0.0/16` | RFC 1918 private |
| `127.0.0.0 - 127.255.255.255` | `127.0.0.0/8` | Loopback |
| `169.254.0.0 - 169.254.255.255` | `169.254.0.0/16` | Link-local / APIPA |
| `100.64.0.0 - 100.127.255.255` | `100.64.0.0/10` | Carrier-grade NAT (RFC 6598) |

The carrier-grade NAT range (`100.64.0.0/10`) is particularly important because:
- Tailscale uses `100.64.0.0/10` for its VPN mesh
- Cloud providers use it for internal networking
- It could expose internal services that appear to have "public" IPs

### IPv6 Private Ranges

| Range | CIDR | Description |
|-------|------|-------------|
| `::1` | `::1/128` | Loopback |
| `::` | `::/128` | Unspecified |
| `fc00:: - fdff::` | `fc00::/7` | Unique Local Addresses (ULA) |
| `fe80::` | `fe80::/10` | Link-local |
| `fec0::` | `fec0::/10` | Site-local (deprecated but still blocked) |
| `fd00:ec2::254` | specific | AWS IMDSv2 IPv6 endpoint |

### IPv4-Mapped IPv6

IPv6 addresses in the `::ffff:0:0/96` range (e.g. `::ffff:192.168.1.1`) are IPv4-mapped addresses. The SSRF protection extracts the embedded IPv4 address and applies all IPv4 rules to it.

This prevents bypass attempts like:
- `http://[::ffff:127.0.0.1]/` (maps to localhost)
- `http://[::ffff:169.254.169.254]/` (maps to cloud metadata)
- `http://[::ffff:10.0.0.1]/` (maps to private network)

## Design Decisions

1. **Pre-resolution blocking** — Checks happen on the parsed URL before making the HTTP request. This means DNS rebinding attacks (where a hostname resolves to a private IP) are not blocked at this layer. The `reqwest` client's redirect policy (limited to 3) provides some mitigation.

2. **Conservative defaults** — When in doubt, block. The agent can always be given direct access to internal services through the config if needed.

3. **No DNS resolution** — We check the hostname string, not the resolved IP. This is a pragmatic trade-off: DNS resolution would catch rebinding attacks but adds latency and complexity. The hostname-based checks catch the vast majority of SSRF attempts.

4. **Carrier-grade NAT** — Blocking `100.64.0.0/10` is a deliberate security choice. While these addresses are technically "shared" (not private per RFC 1918), they are widely used for internal infrastructure and should not be accessible from an AI agent.
