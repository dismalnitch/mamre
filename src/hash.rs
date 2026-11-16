use core::result::Result;
use core::result::Result::{Err, Ok};
pub fn poly_hash(items: &[u64], base: u64) -> Result<u64, u64> {
    if base >= P {
        return Err(0);
    }

    let mut hash: u64 = 0;

    for &c in items {
        hash = add(mul(hash, base), c);
    }
    Ok(hash)
}

const P: u64 = (1 << 61) - 1;
fn mul(a: u64, b: u64) -> u64 {
    let x = a as u128 * b as u128;
    let r = (x as u64 & P) + (x >> 61) as u64;
    if r >= P { r - P } else { r }
}

fn add(a: u64, b: u64) -> u64 {
    let r = a + b;
    if r >= P { r - P } else { r }
}

pub fn pow(mut b: u64, mut k: u64) -> u64 {
    let mut r = 1;
    while k > 0 {
        if k & 1 == 1 {
            r = mul(r, b);
        }
        b = mul(b, b);
        k >>= 1;
    }
    r
}
