# Benchmark config for bench/admin.sh — the operator surface.
#
# The admin listener gets its own port, deliberately. It publishes backend
# health, cache occupancy and the configuration generation, none of which
# belongs on the address that serves the public.
version: 1
server:
  listen: "127.0.0.1:@LISTEN@"
  graceful:
    pid_file: "@PIDFILE@"
    upgrade_socket: "@UPGRADESOCK@"
    drain_period: 2s
    shutdown_timeout: 10s
origin:
  upstreams: ["127.0.0.1:@ORIGIN@"]
  concurrency:
    max: 50
cache:
  enabled: false
telemetry:
  admin:
    listen: "127.0.0.1:@ADMIN@"
  prometheus:
    listen: "127.0.0.1:@METRICS@"
routes:
  - id: pages
    match: "/**"
    class: public_ssr
    concurrency:
      max: 4
