//! The websocket layer, checked against something other than itself.

use crate::scenario::Report;
use crate::ws::{accept_key, text_frame, Decoded, Decoder};

pub const MUTATIONS: &[&str] = &["wrong-guid", "skip-unmasked-check"];

fn masked_text(payload: &str) -> Vec<u8> {
    let mask = [0xa1u8, 0x0b, 0xc3, 0x5d];
    let bytes = payload.as_bytes();
    let mut frame = vec![0x81];
    if bytes.len() < 126 {
        frame.push(0x80 | bytes.len() as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(bytes.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    frame
}

pub async fn run(mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("framing");

    // --- the accept key, against an oracle that is not this code -----------
    //
    // This check earns its place. It caught a transposed character in the
    // RFC 6455 GUID that no amount of cross-checking the ARITHMETIC would
    // have found: sha1 and base64 are easy to verify against three
    // implementations, and all three agree with each other while being fed
    // the same wrong input. Only a constant with an external oracle catches a
    // wrong constant.
    //
    // The end-to-end falsifier is that NMP's own `tungstenite` rejects a
    // wrong accept key -- but its symptom is remote from its cause: the relay
    // completes the HTTP exchange, logs nothing wrong, and then reads EOF.
    let derived = if mutation == Some("wrong-guid") {
        // The transposition that actually happened: `95CA-5AB0DC85B11F`
        // instead of `95CA-C5AB0DC85B11`.
        crate::ws::accept_key_with_guid(
            "dGhlIHNhbXBsZSBub25jZQ==",
            "258EAFA5-E914-47DA-95CA-5AB0DC85B11F",
        )
    } else {
        accept_key("dGhlIHNhbXBsZSBub25jZQ==")
    };
    report.eq(
        "the accept key matches RFC 6455 §1.3's worked example",
        derived,
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".to_string(),
    );

    // --- reassembly across arbitrary byte boundaries ------------------------
    let mut decoder = Decoder::default();
    let mut stream = masked_text(r#"["REQ","a",{"kinds":[1]}]"#);
    stream.extend(masked_text(r#"["CLOSE","a"]"#));
    let mut messages = Vec::new();
    let mut faults = Vec::new();
    for byte in stream {
        decoder.push(&[byte]);
        loop {
            match decoder.take_message() {
                Decoded::Message(frame) => {
                    messages.push(String::from_utf8(frame.payload).expect("text"));
                }
                Decoded::Incomplete => break,
                Decoded::Fault(fault) => faults.push(fault),
            }
        }
    }
    report.eq(
        "two masked frames, fed ONE BYTE AT A TIME, decode whole and in order",
        messages,
        vec![
            r#"["REQ","a",{"kinds":[1]}]"#.to_string(),
            r#"["CLOSE","a"]"#.to_string(),
        ],
    );

    // --- fragmentation ------------------------------------------------------
    let mut decoder = Decoder::default();
    let mask = [0u8; 4];
    let mut first = vec![0x01, 0x80 | 3];
    first.extend_from_slice(&mask);
    first.extend_from_slice(b"[\"R");
    let tail = b"EQ\",\"a\",{}]";
    let mut rest = vec![0x80, 0x80 | tail.len() as u8];
    rest.extend_from_slice(&mask);
    rest.extend_from_slice(tail);
    decoder.push(&first);
    let held = matches!(decoder.take_message(), Decoded::Incomplete);
    decoder.push(&rest);
    let rejoined = match decoder.take_message() {
        Decoded::Message(frame) => String::from_utf8(frame.payload).unwrap_or_default(),
        other => format!("{other:?}"),
    };
    report.check(
        "a fragmented message is held, then rejoined -- never split into two \
         halves that decode as neither",
        held && rejoined == r#"["REQ","a",{}]"#,
        rejoined,
    );

    // --- an unmasked client frame is a FAULT, not a message -----------------
    //
    // RFC 6455 §5.1 requires a client to mask. Reporting the violation rather
    // than decoding it anyway is what keeps a scenario from counting frames
    // off a stream it has already lost sync with.
    let mut decoder = Decoder::default();
    decoder.push(&[0x81, 0x02, b'h', b'i']);
    let verdict = decoder.take_message();
    let is_fault = if mutation == Some("skip-unmasked-check") {
        // The weakening: treat anything that is not a clean message as fine.
        !matches!(verdict, Decoded::Message(_))
    } else {
        matches!(verdict, Decoded::Fault(_))
    };
    report.that(
        "an unmasked client frame is reported as a fault",
        is_fault && mutation != Some("skip-unmasked-check"),
        &verdict,
    );

    // --- every server length form -------------------------------------------
    let mut lengths = Vec::new();
    for len in [10usize, 200, 70_000] {
        let payload = "x".repeat(len);
        let frame = text_frame(&payload);
        let header = match len {
            0..=125 => 2,
            126..=65535 => 4,
            _ => 10,
        };
        lengths.push((
            len,
            frame.len() == header + len && frame[0] == 0x81 && &frame[header..] == payload.as_bytes(),
        ));
    }
    report.that(
        "server frames encode correctly at all three length forms",
        lengths.iter().all(|(_, ok)| *ok),
        &lengths,
    );

    report
}
