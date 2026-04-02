# Stateform

**A high-performance API gateway with a Rust data plane and a Go control plane.**

Stateform handles the hard parts of running services at scale — TLS termination, authentication, rate limiting, caching, load balancing, and zero-downtime config reloads — without asking you to operate a monolith you don't control.

---

## What It Is

Stateform is a programmable API gateway split into two distinct processes:

| Component | Language | Job |
|---|---|---|
| **Data plane** (`gateway-proxy`) | [Rust](https://www.rust-lang.org/) | Accepts connections, runs every request through the pipeline, proxies to upstreams |
| **Control plane** (`control-plane`) | [Go](https://go.dev/) | Watches Kubernetes for changes, computes config, streams updates to data planes over gRPC |

The two talk over a persistent gRPC stream. When a Kubernetes Service or Endpoint changes, the control plane recomputes a `ConfigSnapshot` and pushes it to every connected data plane within milliseconds. The data plane swaps the new config in atomically — no restart, no dropped connections.

---

## How It Works

### Request Pipeline

Every inbound request passes through this pipeline in order:

```
Client
  └─▶  TLS termination (rustls)
         └─▶  Router        — match path/method/headers → upstream
                └─▶  Auth         — JWT + API key + mTLS (ALL enabled must pass; fails on first error)
                       └─▶  Rate limiter  — local token bucket → Redis sliding window
                              └─▶  Cache        — L1 DashMap → L2 Redis
                                     └─▶  Upstream proxy — with retry + EWMA LB
                                            └─▶  Response
```

Each stage is independent. You enable or disable auth, cache, and rate limiting per route in `config.yaml`.

### Hot-Reload

Config changes never restart the process:

1. The Kubernetes watcher (using [client-go](https://github.com/kubernetes/client-go) informers) detects a change to a Service or Endpoints object.
2. The Go control plane recomputes the full config and broadcasts a `ConfigSnapshot` proto message on the open gRPC stream.
3. The Rust data plane receives the snapshot and calls `RouterHandle::rebuild()`, which atomically swaps the routing table via [ArcSwap](https://github.com/vorner/arc-swap).
4. In-flight requests finish against the old config. New requests see the new config immediately.

### Two-Tier Rate Limiting

Rate limiting runs at two levels to be both fast and fair:

- **L1 — local token bucket**: in-process, zero network cost, catches obvious bursts before they hit Redis. Built with [DashMap](https://github.com/xacrimon/dashmap) and `std::sync::atomic`.
- **L2 — distributed**: a Lua script executed atomically in [Redis](https://redis.io/) enforces the cluster-wide limit. Two algorithms available: token bucket and sliding window.

This means a single misbehaving client is stopped locally (microseconds), while shared limits across multiple gateway instances are enforced consistently.

### Two-Tier Cache

Response caching works the same way:

- **L1 — in-process LRU**: instant, no serialization. Configurable entry cap.
- **L2 — Redis**: shared across gateway instances, `SETEX`-backed with `Content-Type`-aware JSON serialization.

Cache keys are built from the path, query string, and any `Vary` headers the upstream specifies. Both tiers are checked on read; a miss falls through to the upstream and populates both tiers on the way back.

### Auth Pipeline

Three providers, evaluated in order:

| Provider | Mechanism |
|---|---|
| **JWT** | Bearer token validated against a [JWKS](https://datatracker.ietf.org/doc/html/rfc7517) endpoint. Keys are cached in-process with a configurable TTL. Validates audience, issuer, and expiry. |
| **API Key** | `X-Api-Key` header looked up via `GET stateform:apikey:<key>` in Redis. Store your keys however you like — Stateform just reads them. |
| **mTLS** | Client certificate validated by [rustls](https://github.com/rustls/rustls) at the TLS handshake layer. The verified cert is passed downstream as a request extension. |

Each route declares which providers are required. All enabled providers must pass — a route with `jwt: true, mtls: true` requires a valid JWT **and** a valid client certificate. The pipeline short-circuits on the first failure so it never wastes a Redis round-trip when the JWT is already invalid. A route with no auth block is fully open.

### Load Balancing

Three algorithms, per upstream:

| Algorithm | Use case |
|---|---|
| **Weighted Round Robin** | Stateless services where you want proportional traffic distribution |
| **Least Connections + EWMA** | Compute-heavy workloads where response time varies; selects the endpoint with the lowest `active_connections × ewma_latency_ms` score |
| **Consistent Hashing (FNV-1a)** | Stateful services (sessions, caches) where the same key must always hit the same backend |

### Graceful Shutdown

Stateform uses a two-phase shutdown coordinated by `ShutdownCoordinator`:

1. **Signal phase**: SIGTERM or Ctrl-C broadcasts on a `tokio::sync::watch` channel. The accept loop stops accepting new connections.
2. **Drain phase**: An `Arc<AtomicUsize>` counter tracks in-flight requests. Each request holds a `DrainGuard`; dropping it decrements the counter. Shutdown completes when the counter hits zero, or after a 30-second timeout.

No request is cut off mid-flight.

---

## Tech Stack

### Rust — Data Plane

| Crate | What it does | Why not X? |
|---|---|---|
| [tokio](https://tokio.rs/) | Async runtime | The de facto standard for async Rust. Purpose-built for I/O-bound workloads. |
| [hyper](https://hyper.rs/) | HTTP/1.1 + HTTP/2 | Powers the underlying HTTP layer in dozens of production proxies. Minimal, correct. |
| [hyper-util](https://github.com/hyperium/hyper-util) | Connection pooling, keep-alive | Official companion to hyper for higher-level connection management. |
| [rustls](https://github.com/rustls/rustls) | TLS | Memory-safe, no OpenSSL dependency, audited. No CVE history from C string parsing bugs. |
| [tokio-rustls](https://github.com/rustls/tokio-rustls) | Async TLS integration | Thin bridge between rustls and tokio. |
| [arc-swap](https://github.com/vorner/arc-swap) | Lock-free hot-reload | Swaps `Arc<T>` atomically without a write lock. Readers never block. |
| [dashmap](https://github.com/xacrimon/dashmap) | Concurrent hash maps | Sharded `RwLock<HashMap>` with a better API. Used for L1 cache and rate limit state. |
| [redis](https://github.com/redis-rs/redis-rs) | Redis client | Async, supports connection pooling and Lua scripts. |
| [jsonwebtoken](https://github.com/Keats/jsonwebtoken) | JWT validation | Pure Rust, supports RS256/ES256, JWKS key rotation. |
| [prometheus](https://github.com/tikv/rust-prometheus) | Metrics | Private registry (no global state leaking between tests), Prometheus text format. |
| [tracing](https://github.com/tokio-rs/tracing) | Structured logging | Async-aware spans and events. Works with tokio's task model. |
| [serde](https://serde.rs/) | Serialization | Zero-cost deserialization, derives for config structs. |
| [thiserror](https://github.com/dtolnay/thiserror) | Error types | Ergonomic `Error` derive. No runtime overhead. |

**Why Rust for the data plane?**

The data plane is on the hot path of every request. Rust gives you:

- **Predictable latency**: no GC pauses, no stop-the-world events.
- **Memory safety without a runtime**: no null pointer bugs, no use-after-free, enforced by the compiler.
- **True concurrency**: `tokio` runs thousands of connections on a handful of threads with zero blocking.

Alternatives considered:

| Alternative | Reason not chosen |
|---|---|
| [NGINX](https://nginx.org/) | Config is not programmable. Extending it means writing C modules or Lua. |
| [Envoy](https://www.envoyproxy.io/) | Powerful but heavy. WASM filters add latency. Config API is verbose. Designed for service mesh, not standalone gateway. |
| [Kong](https://konghq.com/) | Plugin model runs Lua on top of NGINX. Open-source edition lacks many enterprise features. |
| [Traefik](https://traefik.io/) | Go GC introduces latency spikes under load. Good for simple routing, not composable auth pipelines. |
| Go (for the data plane too) | GC pauses are measurable at p99. Rust gives lower and more consistent tail latency. |

---

### Go — Control Plane

| Package | What it does |
|---|---|
| [client-go](https://github.com/kubernetes/client-go) | Kubernetes informers — watches Services and Endpoints in real time |
| [google.golang.org/grpc](https://grpc.io/) | gRPC server — streams `ConfigSnapshot` to data planes |
| [google.golang.org/protobuf](https://pkg.go.dev/google.golang.org/protobuf) | Proto message serialization |
| [go.uber.org/zap](https://github.com/uber-go/zap) | Structured logging |
| [github.com/go-chi/chi/v5](https://github.com/go-chi/chi) | Admin HTTP API router |

**Why Go for the control plane?**

The control plane is not on the request path. It runs informer loops, computes config diffs, and manages gRPC streams — all I/O-bound and latency-tolerant. Go's concurrency model (goroutines + channels) maps cleanly onto the informer pattern, and the Kubernetes ecosystem is primarily Go. Using Go here means we can use `client-go` directly without a translation layer.

---

### Infrastructure

| Tool | Role |
|---|---|
| [Redis](https://redis.io/) | Distributed rate limiting (Lua scripts), response cache L2, API key store |
| [Prometheus](https://prometheus.io/) | Metrics collection — gateway exposes `/metrics` on port 9090 |
| [Grafana](https://grafana.com/) | Dashboards — pre-built dashboard ships with the repo |
| [Docker Compose](https://docs.docker.com/compose/) | Local monitoring stack (Prometheus + Grafana) |
| [Kubernetes](https://kubernetes.io/) | Production deployment target; control plane watches K8s API directly |
| [protoc](https://grpc.io/docs/protoc-installation/) + [protoc-gen-go](https://pkg.go.dev/google.golang.org/protobuf/cmd/protoc-gen-go) + [protoc-gen-go-grpc](https://pkg.go.dev/google.golang.org/grpc/cmd/protoc-gen-go-grpc) | Proto code generation for the gRPC contract |

---

## Who Is It For

**Platform and infrastructure teams** running microservices on Kubernetes who need:

- A gateway they can reason about — the code is readable Rust and Go, not NGINX config macros.
- Zero-downtime deploys — config changes stream to the data plane in real time, no pod restarts.
- Composable auth — mix JWT, API keys, and mTLS per route, not per gateway instance.
- Predictable performance — no GC spikes, no Lua interpreter overhead on the hot path.
- First-class observability — Prometheus metrics and structured logs out of the box.

**Not the right tool if:**

- You need a GUI to manage routes (use Kong or AWS API Gateway).
- You're not on Kubernetes (the control plane is Kubernetes-native).
- You need a service mesh (use Envoy/Istio for that layer).

---

## Configuration

The full reference configuration lives in [`config.yaml`](./config.yaml). Key sections:

```yaml
listener:
  bind: "0.0.0.0:8080"
  tls:
    cert_path: /certs/server.crt
    key_path:  /certs/server.key
    client_ca_path: /certs/ca.pem  # enables mTLS

upstreams:
  api-cluster:
    endpoints:
      - address: "10.0.0.1:8000"
        weight: 10
    load_balancing:
      algorithm: weighted_round_robin
    retry:
      max_attempts: 2
      on_statuses: [502, 503, 504]

routes:
  - id: api-authenticated
    match_:
      path:
        type: prefix
        value: /v1/api
    upstream: api-cluster
    middleware:
      auth:
        jwt: true
      rate_limit:
        policy: strict
      cache:
        ttl: 30
```

---

## Observability

Stateform exposes Prometheus metrics on `:9090/metrics`. A pre-built Grafana dashboard is included at `control-plane/stateform-gateway.json`.

Spin up the local monitoring stack:

```bash
docker compose -f docker-compose.monitoring.yml up -d
```

Then open [http://localhost:3000](http://localhost:3000) — Grafana with the Stateform dashboard pre-loaded.

Metrics tracked:

- Requests per second, by route and status code
- Upstream latency (p50, p95, p99)
- Cache hit rate (L1 and L2)
- Rate limit rejections, by policy
- Auth failures, by provider
- Active connections
- Config reload success/failure count

---

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) (stable, 1.75+)
- [Go](https://go.dev/dl/) (1.22+)
- [protoc](https://grpc.io/docs/protoc-installation/) with `protoc-gen-go` and `protoc-gen-go-grpc`
- [Redis](https://redis.io/) (local or remote)
- [Docker](https://www.docker.com/) (for the monitoring stack)

### Build the data plane

```bash
cargo build --release -p gateway-proxy
```

### Build the control plane

```bash
cd control-plane
go build ./cmd/control-plane
```

### Run locally

```bash
# Start Redis
redis-server

# Start the data plane
./target/release/gateway-proxy --config config.yaml

# Start the control plane (requires kubeconfig)
./control-plane/control-plane
```

---

## License

MIT
