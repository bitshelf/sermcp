//! Compare text-only decoding with the offset-producing path on serial logs.

use std::hint::black_box;
use std::time::{Duration, Instant};

use sermcp::ansi_strip;

fn measure(input: &[u8], decode: impl Fn(&[u8]) -> Vec<u8>) -> Duration {
    for _ in 0..100 {
        black_box(decode(black_box(input)));
    }
    let start = Instant::now();
    for _ in 0..10_000 {
        black_box(decode(black_box(input)));
    }
    start.elapsed()
}

fn main() {
    for (name, line) in [
        (
            "plain",
            "[    1.234567] Linux booting: console=ttyS2,115200\r\n",
        ),
        (
            "ansi_utf8",
            "\x1b[32m[ OK ]\x1b[0m 启动服务\r\n\x1b]0;console\x07root@board:~# ",
        ),
    ] {
        let input = line.repeat(80).into_bytes();
        assert_eq!(
            ansi_strip::strip(&input),
            ansi_strip::strip_with_offsets(&input).0
        );
        for round in 0..5 {
            let with_offsets = || measure(&input, |bytes| ansi_strip::strip_with_offsets(bytes).0);
            let text_only = || measure(&input, ansi_strip::strip);
            let (old, new) = if round % 2 == 0 {
                (with_offsets(), text_only())
            } else {
                let new = text_only();
                (with_offsets(), new)
            };
            println!(
                "{name} round={round} bytes={} offsets={old:?} text_only={new:?} ratio={:.2}",
                input.len(),
                old.as_secs_f64() / new.as_secs_f64()
            );
        }
    }
}
