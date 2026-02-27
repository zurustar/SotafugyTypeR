use criterion::{criterion_group, criterion_main, Criterion};
use sip_load_test::sip::formatter::{format_sip_message, format_into_pooled};
use sip_load_test::sip::message::Headers;
use sip_load_test::sip::parser::{parse_sip_message, parse_sip_message_pooled};
use sip_load_test::sip::pool::MessagePool;

/// INVITE リクエストのサンプルメッセージ
const INVITE_MSG: &[u8] = b"INVITE sip:bob@example.com SIP/2.0\r\n\
    Via: SIP/2.0/UDP 192.168.1.1:5060;branch=z9hG4bK776asdhds\r\n\
    From: <sip:alice@example.com>;tag=1928301774\r\n\
    To: <sip:bob@example.com>\r\n\
    Call-ID: a84b4c76e66710@pc33.example.com\r\n\
    CSeq: 314159 INVITE\r\n\
    Content-Length: 0\r\n\
    \r\n";

/// REGISTER リクエストのサンプルメッセージ
const REGISTER_MSG: &[u8] = b"REGISTER sip:registrar.example.com SIP/2.0\r\n\
    Via: SIP/2.0/UDP 192.168.1.1:5060;branch=z9hG4bKnashds7\r\n\
    From: <sip:alice@example.com>;tag=a73kszlfl\r\n\
    To: <sip:alice@example.com>\r\n\
    Call-ID: 1j9FpLxk3uxtm8tn@192.168.1.1\r\n\
    CSeq: 1 REGISTER\r\n\
    Contact: <sip:alice@192.168.1.1:5060>\r\n\
    Content-Length: 0\r\n\
    \r\n";

/// 200 OK レスポンスのサンプルメッセージ
const OK_200_MSG: &[u8] = b"SIP/2.0 200 OK\r\n\
    Via: SIP/2.0/UDP 192.168.1.1:5060;branch=z9hG4bK776asdhds\r\n\
    From: <sip:alice@example.com>;tag=1928301774\r\n\
    To: <sip:bob@example.com>;tag=a6c85cf\r\n\
    Call-ID: a84b4c76e66710@pc33.example.com\r\n\
    CSeq: 314159 INVITE\r\n\
    Content-Length: 0\r\n\
    \r\n";

fn bench_parse_sip_message(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_sip_message");

    group.bench_function("parse_invite", |b| {
        b.iter(|| parse_sip_message(criterion::black_box(INVITE_MSG)))
    });

    group.bench_function("parse_register", |b| {
        b.iter(|| parse_sip_message(criterion::black_box(REGISTER_MSG)))
    });

    group.bench_function("parse_200_ok", |b| {
        b.iter(|| parse_sip_message(criterion::black_box(OK_200_MSG)))
    });

    group.finish();
}

fn bench_format_sip_message(c: &mut Criterion) {
    let invite = parse_sip_message(INVITE_MSG).expect("INVITE parse failed");
    let register = parse_sip_message(REGISTER_MSG).expect("REGISTER parse failed");
    let ok_200 = parse_sip_message(OK_200_MSG).expect("200 OK parse failed");

    let mut group = c.benchmark_group("format_sip_message");

    group.bench_function("format_invite", |b| {
        b.iter(|| format_sip_message(criterion::black_box(&invite)))
    });

    group.bench_function("format_register", |b| {
        b.iter(|| format_sip_message(criterion::black_box(&register)))
    });

    group.bench_function("format_200_ok", |b| {
        b.iter(|| format_sip_message(criterion::black_box(&ok_200)))
    });

    group.finish();
}

/// Headers::get() のベンチマーク - 頻出ヘッダ vs 非頻出ヘッダ
fn bench_headers_get(c: &mut Criterion) {
    // ベンチマーク用のHeadersを構築（典型的なINVITEメッセージのヘッダ構成）
    let mut headers = Headers::new();
    headers.add("Via", "SIP/2.0/UDP 192.168.1.1:5060;branch=z9hG4bK776asdhds".to_string());
    headers.add("From", "<sip:alice@example.com>;tag=1928301774".to_string());
    headers.add("To", "<sip:bob@example.com>".to_string());
    headers.add("Call-ID", "a84b4c76e66710@pc33.example.com".to_string());
    headers.add("CSeq", "314159 INVITE".to_string());
    headers.add("Contact", "<sip:alice@192.168.1.1:5060>".to_string());
    headers.add("Max-Forwards", "70".to_string());
    headers.add("User-Agent", "SipLoadTest/1.0".to_string());
    headers.add("Content-Type", "application/sdp".to_string());
    headers.add("Content-Length", "0".to_string());

    let mut group = c.benchmark_group("headers_get");

    // 頻出ヘッダ（将来のインデックスキャッシュ最適化対象）
    group.bench_function("frequent_via", |b| {
        b.iter(|| headers.get(criterion::black_box("Via")))
    });

    group.bench_function("frequent_from", |b| {
        b.iter(|| headers.get(criterion::black_box("From")))
    });

    group.bench_function("frequent_to", |b| {
        b.iter(|| headers.get(criterion::black_box("To")))
    });

    group.bench_function("frequent_call_id", |b| {
        b.iter(|| headers.get(criterion::black_box("Call-ID")))
    });

    group.bench_function("frequent_cseq", |b| {
        b.iter(|| headers.get(criterion::black_box("CSeq")))
    });

    // 非頻出ヘッダ（線形探索のまま）
    group.bench_function("infrequent_contact", |b| {
        b.iter(|| headers.get(criterion::black_box("Contact")))
    });

    group.bench_function("infrequent_user_agent", |b| {
        b.iter(|| headers.get(criterion::black_box("User-Agent")))
    });

    group.bench_function("infrequent_content_type", |b| {
        b.iter(|| headers.get(criterion::black_box("Content-Type")))
    });

    group.finish();
}

fn bench_parse_sip_message_pooled(c: &mut Criterion) {
    let pool = MessagePool::new(1);

    let mut group = c.benchmark_group("parse_sip_message_pooled");

    group.bench_function("parse_invite_pooled", |b| {
        b.iter(|| {
            let mut buf = pool.get();
            let result = parse_sip_message_pooled(criterion::black_box(INVITE_MSG), &mut buf);
            pool.put(buf);
            result
        })
    });

    group.finish();
}

fn bench_format_sip_message_pooled(c: &mut Criterion) {
    let invite = parse_sip_message(INVITE_MSG).expect("INVITE parse failed");
    let pool = MessagePool::new(1);

    let mut group = c.benchmark_group("format_sip_message_pooled");

    group.bench_function("format_invite_pooled", |b| {
        b.iter(|| {
            let mut buf = pool.get();
            format_into_pooled(&mut buf, criterion::black_box(&invite));
            pool.put(buf);
        })
    });

    group.finish();
}

fn bench_parse_format_roundtrip_pooled(c: &mut Criterion) {
    let pool = MessagePool::new(1);

    let mut group = c.benchmark_group("parse_format_roundtrip_pooled");

    group.bench_function("parse_format_roundtrip_pooled", |b| {
        b.iter(|| {
            let mut buf = pool.get();
            let msg = parse_sip_message_pooled(criterion::black_box(INVITE_MSG), &mut buf)
                .expect("parse failed");
            format_into_pooled(&mut buf, &msg);
            let _output = &buf.output;
            pool.put(buf);
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_parse_sip_message,
    bench_format_sip_message,
    bench_headers_get,
    bench_parse_sip_message_pooled,
    bench_format_sip_message_pooled,
    bench_parse_format_roundtrip_pooled
);
criterion_main!(benches);
