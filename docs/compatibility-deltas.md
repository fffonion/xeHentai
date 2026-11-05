# Compatibility deltas

## `delta.speed-window-bytes-over-time`

The frozen Python `worker.py:141-171` keeps a count-bounded ring of per-interval rates and returns their arithmetic mean. It appends only after an interval longer than one second and does not expire old samples while idle.

Rust aggregates chunk bytes into fixed 100 ms buckets and calculates throughput from buckets overlapping the trailing `cnt` seconds, divided by `min(cnt, time since reset)`. The boundary resolution is at most one bucket. The display warms up for one second; low-speed classification waits for a complete window. Old buckets expire as time advances, including when no chunks arrive. The idle-queue reset remains in place, and the monitor sums rates from all live download workers.

Verified by the speed-window regression tests in `src/tests.rs` and the `m4.speed.checker` compatibility probe.

## `delta.monitor-speed-cleared-on-exit`

The frozen Python monitor may retain its last nonzero `download_speed` after its loop ends. Rust clears the published value when `Monitor::run` exits, preventing `getInfo` from reporting a stale total. While the monitor is active, the total remains the sum of live worker rates.

Verified by `monitor_sums_worker_speeds_and_clears_the_total_on_exit` in `src/tests.rs` and the `m4.monitor.slow-windows` compatibility probe.
