//! Isolated framing cost, not SMTP throughput. Compare two public APIs in the
//! same optimized binary; reverse measurement order to reduce ordering bias.
use rustymail_protocol::{LineDecoder, decode_data_frame, decode_data_line};
use std::{hint::black_box, time::Instant};

fn measure(line: &[u8], lines: usize, borrowed: bool) -> u128 {
    let mut decoder = LineDecoder::new(1001);
    let started = Instant::now();
    let mut bytes = 0;
    for _ in 0..lines {
        if borrowed {
            decoder.clear();
            assert!(decoder.feed_buffered(black_box(line)).unwrap().1);
            bytes += black_box(
                decode_data_frame(decoder.frame().unwrap())
                    .unwrap()
                    .unwrap(),
            )
            .len();
        } else {
            let mut decoder = LineDecoder::new(1001);
            let frame = decoder.feed(black_box(line)).unwrap().1.unwrap();
            bytes += black_box(decode_data_line(frame).unwrap().unwrap()).len();
        }
    }
    assert_eq!(bytes, line.len() * lines);
    started.elapsed().as_nanos()
}

fn main() {
    println!("{{\"kind\":\"isolated_framing_current_apis\",\"samples\":[");
    let mut separator = "";
    for width in [2, 80, 1000] {
        let mut line = vec![b'x'; width - 2];
        line.extend_from_slice(b"\r\n");
        let lines = 16 * 1024 * 1024 / width;
        for round in 0..4 {
            let (owned_ns, borrowed_ns) = if round % 2 == 0 {
                (measure(&line, lines, false), measure(&line, lines, true))
            } else {
                let borrowed = measure(&line, lines, true);
                (measure(&line, lines, false), borrowed)
            };
            println!(
                "{separator}{{\"line_bytes\":{width},\"lines\":{lines},\"round\":{round},\"owned_ns\":{owned_ns},\"borrowed_ns\":{borrowed_ns}}}"
            );
            separator = ",";
        }
    }
    println!("]}}");
}
