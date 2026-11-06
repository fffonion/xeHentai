# Compatibility deltas

## `delta.proxy-image-stream-integrity`

For streamed image responses, Rust treats an absent `Content-Length` as unknown instead of zero. It accepts an EOF-delimited stream only when the image URL provides an expected byte count, then compares received bytes against that count. HTML error prefixes are rejected whether the response is labelled `image/jpeg` or `text/html`; the staging file is removed and a diagnostic records received/expected byte counts without logging URL or query data. Text responses are inspected for quota notices before rejection: recognized quota notices keep `ERR_QUOTA_EXCEEDED`, while other text bodies are never published. Among body outcomes, only a recognized HTML error body debits proxy health. Text responses, quota notices, complete valid streams and non-integrity pre-stream outcomes record proxy success; retryable failures while fetching response headers continue to follow the transport failure policy. Low-speed aborts, stream read errors, and byte-count mismatches do not increase proxy failures because those conditions do not identify whether the proxy or origin caused the problem.

Verified by the proxy-image integration tests in `src/tests.rs` and
`html_error_body_tests` in `src/core/mod.rs`.

## `delta.i18n-glype-help-text`

The frozen Python locale strings still name `glype` in `XEH_OPT_p`; the Rust proxy parser no longer supports Glype and rejects that scheme. All three locale help strings therefore omit it. The i18n comparator allows only the exact `, glype` removal for that key; all remaining messages, errors, and interpolation outputs remain compared exactly.

Verified by `proxy_help_lists_only_supported_schemes` and the i18n compatibility comparison's Glype negative control.

## `delta.proxy-socks5-remote-dns`

The frozen Python distinguishes `socks5` (local target DNS) from `socks5h` (proxy DNS). Per the requested runtime contract, Rust sends hostname targets as SOCKS5 domain addresses for both schemes, never resolves the target locally, and does not route a selected SOCKS request through the fallback-IP map. This intentional divergence makes the two SOCKS5 schemes behave identically; the proxy endpoint itself still requires local resolution when configured by hostname.

Verified by `socks5_and_socks5h_always_use_remote_dns_without_ip_fallback`, which asserts both schemes send the domain address type, make no target resolver call, and succeed with a fallback IP configured for the target.

## `delta.speed-window-bytes-over-time`

The frozen Python `worker.py:141-171` keeps a count-bounded ring of per-interval rates and returns their arithmetic mean. It appends only after an interval longer than one second and does not expire old samples while idle.

Rust aggregates chunk bytes into fixed 100 ms buckets and calculates throughput from buckets overlapping the trailing `cnt` seconds, divided by `min(cnt, time since reset)`. The boundary resolution is at most one bucket. The display warms up for one second; low-speed classification waits for a complete window measured from the first sample after reset, so an idle gap before a new item does not age that item's window. The checker resets before each download item and when a worker's queue becomes idle. Old buckets expire as time advances, including when no chunks arrive, and the monitor sums rates from all live download workers.

Verified by the speed-window regression tests, `pipeline_resets_speed_window_before_each_download_item` in `src/tests.rs`, and the `m4.speed.checker` compatibility probe.

## `delta.sleep-cancellation`

The frozen Python `HttpWorker.run` performs a one-second blocking wait only after its scan or download queue is empty; a download worker resets its speed checker before waiting. Rust uses the same wait lengths and triggers through the injected clock. No configurable scan/download sleep keys occur in the frozen Python source.

Rust scan/download idle waits are clock-based and cancellation-aware, so pause/cancel can wake them promptly. The task-queue idle poll uses the injected clock, and worker cancellation drops its pending sleep; it has no separate cancellation select. Python's `time.sleep` cannot be interrupted; its `_exit` check runs at the next loop boundary after the wait. This is an intentional cancellation-responsiveness delta, not a timing/configuration mismatch.

Verified by `scan_and_download_idle_waits_are_one_second_and_cancelable` and `task_queue_idle_poll_uses_injected_one_second_sleep`.

## `delta.monitor-speed-cleared-on-exit`

The frozen Python monitor may retain its last nonzero `download_speed` after its loop ends. Rust clears the published value when `Monitor::run` exits, preventing `getInfo` from reporting a stale total. While the monitor is active, the total remains the sum of live worker rates.

Verified by `monitor_sums_worker_speeds_and_clears_the_total_on_exit`, mapped to `m4.monitor.slow-windows` in the compatibility harness. The harness checks lifecycle/event parity separately; the native test owns the published-rate sum and exit-clear assertions.
