# ADR 0003: Implement Per-IP Rate Limiting

## Status

Accepted

## Context

DNS seeders on UDP port 53 are vulnerable to DNS amplification attacks where:

- Attackers forge source IP addresses
- Small queries trigger large responses
- The seeder becomes a DDoS weapon

## Decision

Implement per-IP rate limiting using the `governor` crate:

- 10 queries per second per IP by default, configurable
- Burst capacity of 20 by default
- Silent packet dropping, with no REFUSED response

## Rationale

- **Security**: Prevents amplification attacks
- **Fairness**: No single IP can monopolize resources
- **Performance**: Less than 1 ms overhead with DashMap
- **Configurability**: Operators can tune based on traffic
- **Silent drops**: Avoid amplification from error responses

## Implementation

- `governor` crate for token bucket algorithm, using GCRA
- `DashMap` for per-IP tracking across threads in a single map
- Each IP gets an isolated rate limiter instance
- Metrics track rate-limited requests

## Consequences

- Cannot be weaponized for DDoS
- Fair resource allocation
- Minimal performance impact
- Legitimate high-volume clients may be rate-limited
- Memory grows with unique IPs, which is acceptable at approximately 200 bytes per IP

## Alternatives Considered

- No rate limiting: rejected because it is an unacceptable security risk
- Global rate limit: rejected because one client could deny service to all others
- Response size limiting: rejected because it is insufficient and still allows amplification
