# Benchmark config for bench/chaos.sh.
#
# Two backends so the balancer has somewhere to go when one dies, and health
# checking fast enough that a round of chaos sees a real state change rather
# than the whole outage passing between two probes.
version: 1
server:
  listen: "127.0.0.1:@LISTEN@"
  graceful:
    pid_file: "@PIDFILE@"
    upgrade_socket: "@UPGRADESOCK@"
    drain_period: 1s
    shutdown_timeout: 3s
origin:
  upstreams:
    - "127.0.0.1:@ORIGINA@"
    - "127.0.0.1:@ORIGINB@"
  concurrency:
    max: 8
    queue:
      max: 100
      timeout: 5s
health:
  path: /hot/health
  interval: 1s
  timeout: 1s
  healthy_after: 1
  unhealthy_after: 2
cache:
  enabled: true
  max_memory: 16MiB
telemetry:
  admin:
    listen: "127.0.0.1:@ADMIN@"
  prometheus:
    listen: "127.0.0.1:@METRICS@"
routes:
  - id: hot
    match: "/hot/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 5s
      stale_if_error: 60s
  - id: cold
    match: "/cold/**"
    class: public_ssr
  # No override, no class: a Set-Cookie response, which nothing shares.
  - id: private
    match: "/private/**"
  - id: catchall
    match: "/**"
