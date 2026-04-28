use criterion::{criterion_group, criterion_main, Criterion};

// Phase 1 placeholder. Real pipeline benchmarks land in Phase 2+.
fn bench_noop(c: &mut Criterion) {
    c.bench_function("noop", |b| b.iter(|| std::hint::black_box(42u64)));
}

criterion_group!(benches, bench_noop);
criterion_main!(benches);
