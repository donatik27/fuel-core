use criterion::{
    criterion_group,
    criterion_main,
    Criterion,
};
// TODO: Move from `gas-price-algorithm`
fn gas_price_algo(_c: &mut Criterion) {}

criterion_group!(benches, gas_price_algo);
criterion_main!(benches);