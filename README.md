# rust-rtb-bidder

A standalone Rust DSP bidder, designed for larger scale workload, DSP scale (50K-100K active campaigns, 100M+ user audience, 50 ms p99 SLA).


## Documentation

- [`docs/PLAN.md`](docs/PLAN.md) — full architecture plan, performance targets, workload assumptions, and phase-by-phase breakdown.

## Related projects

- [`RTB-Bidder`](https://github.com/HumbleBee14/RTB-Bidder) — Java implementation of the same workload, running at small scale (1K campaigns)
