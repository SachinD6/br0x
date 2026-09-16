# 0002 Freeze first and park last with adaptive timeouts

Status: accepted.

## Context

Fixed discard timers break audible tabs, calls, downloads, and dirty forms. br0x targets 900 MB to 1.2 GB for 9 tabs without breaking those cases.

## Decision

Run one UI process plus the WebKit multiprocess pool. Freeze background work first. Park full pages last. Adapt all timeouts to tab count and memory pressure. Honor exemptions on every transition.

## Alternatives

- Fixed 60 second discard: rejected. It parks tabs the user still needs and forces painful reloads.
- Freeze only with no park: rejected. It protects correctness but misses the RAM target.

## Consequences

- Frozen tabs switch in under 50 ms. Parked tabs restore from thumbnail in under 1 second.
- Memory pressure shortens timeouts instead of changing rules, so behavior stays predictable.
- Tests verify the strategy through the policy interface without launching a browser.
