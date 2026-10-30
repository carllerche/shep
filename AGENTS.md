# Testing

Do not gate test success on work completing within a given time window, or on
work remaining incomplete for a fixed duration. Time-based tests are very brittle:
a simulated environment such as Weft can advance time while work is still runnable.

Instead, use explicit synchronization, completion events, and state assertions to
test the required ordering and behavior. Design tests so simulated execution (such
as `cargo weft test`) can catch regressions across different schedules. Leave
watchdog timeouts to the test runner rather than embedding them in correctness
assertions. When time itself is the behavior under test, control the clock and
assert the intended timer semantics instead of assuming how quickly work runs.
