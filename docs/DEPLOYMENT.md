# Deployment guide — Linux production

Production target: Linux x86_64 (or aarch64) servers behind an L4 load balancer, fronting a Redis cluster, a Postgres replica, and a Kafka cluster. Most dev was done on macOS Apple Silicon; everything below is what changes when you stop running `cargo run` on a laptop and start running multiple bidders on real hosts.

This guide is **intentionally not a Helm chart**. The Helm design lives at the bottom and is the next deliverable, not this PR. What's here is the kernel/OS/process layer the chart will encode.

---

## 1. Process model

**One bidder process per CPU core, all bound to the same port via `SO_REUSEPORT`.** Linux's kernel routes `accept(2)` round-robin across the listening processes, so we get N independent Tokio runtimes sharing one port without the overhead of work-stealing across cores. PLAN.md § "Phase 7 production default" treats this as the production topology for the Tokio path.

- Each process is single-threaded (`#[tokio::main(flavor = "current_thread")]`) **or** multi-threaded with `worker_threads = 1` — pinned to one CPU.
- A supervisor (systemd template unit, `supervisord`, or the K8s Deployment + a thin `main.rs` parent) launches N copies. K8s prefers one container per pod, so for K8s the model becomes "1 pod per CPU you want to spend on bidding," and `SO_REUSEPORT` becomes intra-pod multi-process only when you deliberately oversubscribe a pod.
- Health endpoints (`/health/live`, `/health/ready`) are unauthenticated and cheap. Use `/health/ready` for the LB's readiness probe and `/health/live` for liveness. Readiness flips to 200 only after the catalog has loaded; liveness is always 200 once the process is up.

Why not multi-threaded Tokio with a wider scheduler? Cross-thread work stealing is real overhead at 30K+ RPS and we'd rather pay context-switch cost on packet boundaries (`SO_REUSEPORT`) than on every awaitable. The `bidder-server-monoio` experimental binary takes the same idea further with `io_uring`; it ships separately when/if profiling shows the syscall path is the limiter.

---

## 2. CPU pinning + NUMA

On a dual-socket box, RAM access across NUMA nodes adds ~80–120 ns per cache miss — meaningful when you're chasing a 50 ms p99 with 30+ Redis round-trips per request.

```bash
# Pin each bidder process to one core, with memory bound to the same NUMA node.
numactl --cpunodebind=0 --membind=0 \
  taskset -c 4 \
  ./bidder-server --config /etc/bidder/config.toml
```

Patterns that work:

- **Single-socket box** (most cloud instances): just `taskset -c <core>` per process. `numactl` is a no-op.
- **Dual-socket bare metal**: split processes across NUMA nodes, never let one process touch RAM on the other node. Match Redis client connections to the same node when the Redis pool is per-process (which it is — see `bidder-server/src/main.rs`).
- **Reserve cores for the kernel/IRQ handlers**. Boot with `isolcpus=4-31` (assuming cores 0–3 stay for the OS) and pin bidder processes onto the isolated set. This trades dev-friendliness for predictable tail latency under load.

---

## 3. NIC / IRQ affinity

The default Linux IRQ balancer spreads NIC interrupts across all cores, which is the opposite of what we want when bidder processes are pinned. Either:

- Stop `irqbalance`, then write the affinity mask manually so NIC RX/TX queues land on cores 0–3 (the OS reservation):
  ```bash
  systemctl stop irqbalance
  for irq in $(grep -E 'eth0|ens|enp' /proc/interrupts | awk '{print $1}' | tr -d ':'); do
    echo f > /proc/irq/$irq/smp_affinity   # cores 0–3
  done
  ```
- Or, if your NIC supports RSS with as many queues as bidder processes, distribute IRQs 1:1 onto the bidder cores so RX completes on the same core that runs the request. Enables the full `SO_REUSEPORT` benefit (no inter-core hop between `accept(2)` and the userspace handler).

Verify: `cat /proc/interrupts | grep <nic>` after a short load test — the column for your bidder cores should grow, the OS-reserved cores' columns should grow only modestly.

---

## 4. Sysctl + TCP tuning

Defaults work for development. Production needs:

```ini
# /etc/sysctl.d/99-bidder.conf
net.core.somaxconn = 65535
net.ipv4.tcp_max_syn_backlog = 65535
net.ipv4.tcp_tw_reuse = 1
net.ipv4.tcp_fin_timeout = 15
net.ipv4.tcp_keepalive_time = 60
net.ipv4.tcp_keepalive_intvl = 10
net.ipv4.tcp_keepalive_probes = 3
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
net.ipv4.tcp_rmem = 4096 87380 16777216
net.ipv4.tcp_wmem = 4096 65536 16777216
fs.file-max = 1048576
```

- `somaxconn` and `tcp_max_syn_backlog` keep the listen queue from dropping bursts when an RPS spike hits. The default `128` is way too low.
- `tcp_tw_reuse` lets new outbound connections (to Redis, to Kafka) reuse `TIME_WAIT` sockets — necessary on the bidder because every request makes 1+ Redis MGET and at high RPS we churn ephemeral ports.
- `fs.file-max` and the per-process `ulimit -n` (set in the systemd unit or pod securityContext) need to comfortably exceed `pool_size_per_redis * num_redis_endpoints + kafka_producer_fds + http_listen + http_keepalive_clients`. 1M is overkill but cheap.

---

## 5. Allocator + huge pages

The bidder uses jemalloc by default (mimalloc is a swap-in for Windows; not relevant in production). On Linux:

- `MALLOC_CONF=background_thread:true,metadata_thp:auto` enables jemalloc's background thread for purging, and lets jemalloc back its metadata with transparent huge pages.
- If you have access to `vm.nr_hugepages` and run on dedicated hardware, allocate enough 2MB hugepages to back the catalog (estimate: 5× catalog row count × avg row size). The catalog hot-reloads via `ArcSwap` so memory churn is infrequent — hugepages amortize across many requests.

Don't enable `bumpalo` arena (`--features allocator-arena`) unless `samply` shows the global allocator is the top hot spot. Phase 4–6 profiling did not show that, so the arena stays off in production.

---

## 6. Config + secrets

Config is `/etc/bidder/config.toml`, mounted read-only. Override via env (`BIDDER__SECTION__KEY=value`). Secrets — Postgres password, win-notice HMAC secrets, Kafka SASL credentials — flow in through env from a secret manager (Vault, AWS Secrets Manager, K8s Secret + projected volume).

The Phase 7 per-SSP HMAC config takes a map:

```toml
[win_notice]
require_auth = true
secret = "default-shared-secret"   # fallback for any SSP not listed below
[win_notice.secrets]
"openrtb-generic" = "..."
"google-adx"      = "..."
"magnite"         = "..."
```

Or via env: `BIDDER__WIN_NOTICE__SECRETS__OPENRTB_GENERIC=...`. Rotate by deploying a new value; there is no in-process rotation API yet.

---

## 7. Observability

The bidder exports Prometheus metrics on port 9090 (`bidder.bid.duration_seconds`, `bidder.qps`, `bidder.redis.*`, `bidder.kafka.events_dropped`, `bidder.hedge.*`, `bidder.kafka.incident_mode_active`, etc.). The `docker-compose.yml` in this repo provisions Prometheus + a "Bidder — Live" Grafana dashboard auto-loaded from `docker/grafana/dashboards/bidder.json`. In K8s, point a `ServiceMonitor` at `:9090/metrics` and import the same dashboard JSON into your shared Grafana.

Tracing is OpenTelemetry-compatible via the `tracing` crate; production points the OTLP exporter at Tempo, Jaeger, or whatever your stack is. The bid-path spans are intentionally short — every stage names itself with the budget contract from `config.toml`.

Two SLO-grade signals to alert on first:

- **`bidder.bid.duration_seconds` p99 > 50 ms for 5 min.** Latency contract; PLAN.md § "Latency budget."
- **`bidder.qps` rate of change** (sudden cliffs). Catches L4 LB misroutes and process death faster than `up == 0`.

Two adaptive-system signals worth a board panel but not page-on:

- **`bidder.kafka.incident_mode_active = 1` for > 10 min.** The incident-mode loop already shed Kafka pressure; investigate the root cause (broker, network) before it turns into a sustained drop.
- **`bidder.hedge.load_shed_rate > 0.05`** sustained. The HTTP layer is rejecting requests; either RPS exceeded `max_concurrency` or downstream Redis is degrading.

---

## 8. Graceful drain

Signal handling: `SIGTERM` triggers a drain — stop accepting new connections (drop the listener), let in-flight requests finish (bounded by the bid-path 50ms timeout), flush the Kafka producer, then exit. K8s sends `SIGTERM` `terminationGracePeriodSeconds` before `SIGKILL`; set that to ~10s, well above the bid-path timeout but short enough to recycle pods quickly.

Phase 5 wired the `HealthState` ready flag; readiness flips to 503 immediately on SIGTERM so the LB stops routing before in-flight drain completes. Liveness stays 200 until the process exits so K8s doesn't kill us mid-drain.

---

## 9. Kernel + libc requirements

- Linux 5.10+ if you want to run the experimental `bidder-server-monoio` binary (io_uring). The default `bidder-server` runs on anything modern.
- glibc-based images. The `ort` crate's `libonnxruntime` is built against glibc; Alpine + musl needs a different ORT build and is not on the supported path.
- `protoc` is bundled by `protoc-bin-vendored` at build time, so production runtime doesn't need it.

---

## 10. Helm chart — design (not implementation)

What the chart needs to express, when we author it:

| Concern | How |
|---|---|
| Replica count | `Deployment.replicas` driven by HPA on `bidder.bid.duration_seconds.p99` and `bidder.qps`. Floor of 3 for AZ resilience. |
| Pod-level CPU model | One bidder process per pod (cleanest K8s pattern). To run multi-process per pod (e.g., to amortize sidecars), drop the bidder behind a small wrapper binary that re-execs N copies bound via `SO_REUSEPORT` — and document the tradeoff loudly. |
| CPU/memory requests | `requests = limits` (Guaranteed QoS). Production starting point: `cpu=1, memory=2Gi` per replica, tune from real load. Catalog memory dominates; size based on row count. |
| AZ spread | `topologySpreadConstraints` with `topologyKey: topology.kubernetes.io/zone`, `whenUnsatisfiable: DoNotSchedule`, `maxSkew: 1`. |
| Disruption | `PodDisruptionBudget.minAvailable = max(1, 0.66 * replicas)`. |
| Secrets | `Secret` mounted as projected volume; values pulled into env via `envFrom` so the bidder reads them as `BIDDER__...`. |
| Config | `ConfigMap` mounted read-only at `/etc/bidder/config.toml`. Hot-reload via SIGHUP is **not** wired yet (Phase 8); for now every config change is a rolling deploy. |
| Health probes | `livenessProbe` → `/health/live` 1s timeout 5s period; `readinessProbe` → `/health/ready` same cadence. `startupProbe` with a generous failureThreshold to cover catalog warm. |
| Metrics scrape | `ServiceMonitor` (Prometheus Operator) on `:9090/metrics`, scrape every 5s — same cadence as the local docker-compose Prometheus. |
| Tracing | OTLP env vars piped through `Deployment.env` (`OTEL_EXPORTER_OTLP_ENDPOINT`). |
| Sysctl | `securityContext.sysctls` for `net.core.somaxconn` etc. K8s allows a small whitelist by default; the rest go in node-level kubelet flags or a privileged init container. |
| File descriptors | `securityContext.runAsNonRoot: true` plus `ulimits.nofile = 65536` via the runtime class or a privileged init that does `ulimit -n` and execs the bidder. |
| Kernel tuning that doesn't fit in K8s | Documented as a node-init `DaemonSet` or kubelet flags — IRQ affinity, isolcpus, hugepages. K8s won't manage these for you. |

The chart **deliberately does not own** Redis, Postgres, or Kafka — those are external dependencies expressed as required `values.yaml` URLs. The bidder talks to whatever the platform provides; the chart's job is the bidder process, not the infrastructure around it.

---

## What's not in this PR

- An actual Helm chart (just the design above). Phase 8 deliverable.
- A node-init `DaemonSet` for IRQ affinity + isolcpus + hugepages. Phase 8.
- The monoio experimental binary's deployment story. Separate doc when/if the binary ships as production.
