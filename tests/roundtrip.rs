//! Round trips on edge cases and random data, and rejection of damaged input.

fn rt(data: &[u8]) -> usize {
    let c = osmium::compress(data, |_| {});
    let d = osmium::decompress(&c, |_| {}).expect("decompress");
    assert_eq!(d, data, "round trip of {} bytes", data.len());
    c.len()
}

#[test]
fn edge_cases() {
    rt(b"");
    rt(b"a");
    rt(&[0u8; 1]);
    rt(&[255u8; 100_000]);
    rt(&(0..=255u8).collect::<Vec<_>>());
}

#[test]
fn random_bytes_do_not_grow_much() {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let data: Vec<u8> = (0..200_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let n = rt(&data);
    assert!(
        n < data.len() + data.len() / 50 + 64,
        "random data grew to {n}"
    );
}

#[test]
fn text_compresses() {
    let text = "The quick brown fox jumps over the lazy dog. ".repeat(2000);
    let n = rt(text.as_bytes());
    assert!(n < text.len() / 50, "repetitive text only reached {n}");
}

#[test]
fn rejects_bad_input() {
    assert_eq!(
        osmium::decompress(b"nope", |_| {}),
        Err(osmium::Error::NotOsmium)
    );
    let c = osmium::compress(
        "hello world, hello world, hello world"
            .repeat(50)
            .as_bytes(),
        |_| {},
    );
    assert_eq!(
        osmium::decompress(&c[..c.len() / 2], |_| {}),
        Err(osmium::Error::Truncated)
    );
    let mut huge = c.clone();
    huge[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(
        osmium::decompress(&huge, |_| {}),
        Err(osmium::Error::Truncated)
    );
}
