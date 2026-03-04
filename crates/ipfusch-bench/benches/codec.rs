use criterion::{Criterion, black_box, criterion_group, criterion_main};
use ipfusch_core::protocol::DataHeader;

fn bench_header_codec(c: &mut Criterion) {
    c.bench_function("data_header_encode_decode", |b| {
        b.iter(|| {
            let h = DataHeader::new(42, 3, 99, 123_456_789, 1400);
            let bytes = h.encode();
            let decoded = DataHeader::decode(black_box(&bytes)).expect("decode header");
            black_box(decoded)
        })
    });
}

criterion_group!(benches, bench_header_codec);
criterion_main!(benches);
