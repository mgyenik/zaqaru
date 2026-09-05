//! The JSON reader, which exists to get one list right.
//!
//! An image's layer order comes from `manifest.json`, and reading the wrong
//! list is the failure that matters: an image built from layers in the wrong
//! order, or from a different image's layers, is subtly and silently not the
//! one that was asked for. Nothing downstream can tell. So the reader refuses
//! what it does not understand, and these are the cases where a scanner
//! would have guessed instead.

use image::json::{Value, parse};

/// The shape `docker save` actually writes — copied from docker 29.1.3's
/// output for `alpine:3.20`, digests and all.
const REAL: &str = r#"[{"Config":"blobs/sha256/bf8527eb54c3680e728d5b4b383a8ba730d72dae7236fbc8dff97ed6b224a731","RepoTags":["alpine:3.20"],"Layers":["blobs/sha256/25f1d6b1951ac8eb3740558fe94cb83d377bdadf95fd9f98b50d2e1b96130471"]}]"#;

#[test]
fn a_real_manifest_reads_back_as_itself() {
    let document = parse(REAL.as_bytes()).expect("parse");
    let images = document.as_array().expect("an array");
    assert_eq!(images.len(), 1);
    let layers = images[0]
        .get("Layers")
        .expect("Layers")
        .as_array()
        .expect("an array");
    assert_eq!(layers.len(), 1);
    assert_eq!(
        layers[0].as_str(),
        Some("blobs/sha256/25f1d6b1951ac8eb3740558fe94cb83d377bdadf95fd9f98b50d2e1b96130471")
    );
    assert_eq!(
        images[0]
            .get("RepoTags")
            .expect("RepoTags")
            .as_array()
            .expect("array")[0]
            .as_str(),
        Some("alpine:3.20")
    );
    assert!(images[0].get("Nothing").is_none());
}

/// The reason this is a parser and not a search. A repository tag, a
/// container's command line, or a label is free text, and free text can
/// contain anything a manifest's own syntax contains.
#[test]
fn a_string_that_looks_like_the_key_is_not_the_key() {
    let document = parse(
        br#"[{"RepoTags":["evil:\"Layers\":[\"blobs/sha256/wrong\"]"],
             "Layers":["blobs/sha256/right"]}]"#,
    )
    .expect("parse");
    let images = document.as_array().expect("array");
    let layers = images[0]
        .get("Layers")
        .expect("Layers")
        .as_array()
        .expect("array");
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].as_str(), Some("blobs/sha256/right"));
}

#[test]
fn the_values_json_has_all_read_back() {
    let source = concat!(
        r#"{"null":null,"yes":true,"no":false,"number":-12.5e3,"#,
        r#""escapes":"a\"b\\c\/d\be\ff\ng\rh\tiAj","#,
        // Raw multi-byte UTF-8 in the document, not an escape.
        "\"utf8\":\"caf\u{e9} \u{fc} \u{2713}\",",
        r#""escaped":"caf\u00e9 \u00fc","#,
        r#""empty":{},"list":[],"nested":[[1,[2]]]}"#
    );
    let document = parse(source.as_bytes()).expect("parse");
    assert_eq!(document.get("null"), Some(&Value::Null));
    assert_eq!(document.get("yes"), Some(&Value::Bool(true)));
    assert_eq!(document.get("no"), Some(&Value::Bool(false)));
    assert_eq!(
        document.get("number"),
        Some(&Value::Number("-12.5e3".to_string()))
    );
    assert_eq!(
        document.get("escapes").and_then(Value::as_str),
        Some("a\"b\\c/d\u{8}e\u{c}f\ng\rh\ti\u{41}j")
    );
    assert_eq!(
        document.get("utf8").and_then(Value::as_str),
        Some("caf\u{e9} \u{fc} \u{2713}")
    );
    // And the same characters written as `\u` escapes.
    assert_eq!(
        document.get("escaped").and_then(Value::as_str),
        Some("caf\u{e9} \u{fc}")
    );
    assert_eq!(document.get("empty"), Some(&Value::Object(Vec::new())));
    assert_eq!(document.get("list"), Some(&Value::Array(Vec::new())));
    assert_eq!(
        document
            .get("nested")
            .and_then(Value::as_array)
            .map(<[Value]>::len),
        Some(1)
    );
}

/// Each of these is a document some reader somewhere accepts, and each would
/// mean the manifest said something it did not.
#[test]
fn what_is_not_json_is_refused() {
    for (label, document) in [
        ("trailing content", r#"{"a":1} and then some"#),
        ("unterminated string", r#"{"a":"unfinished"#),
        ("unterminated object", r#"{"a":1"#),
        ("missing colon", r#"{"a" 1}"#),
        ("missing comma", r#"{"a":1 "b":2}"#),
        ("trailing comma", r#"{"a":1,}"#),
        ("single quotes", r#"{'a':1}"#),
        ("unquoted key", r#"{a:1}"#),
        ("bad escape", r#"{"a":"\q"}"#),
        ("truncated escape", r#"{"a":"\u00"}"#),
        ("raw newline in a string", "{\"a\":\"one\ntwo\"}"),
        ("not a value", "{"),
        ("empty document", ""),
        ("a bare word", "yes"),
    ] {
        assert!(
            parse(document.as_bytes()).is_err(),
            "`{label}` was accepted: {document}"
        );
    }
    // Not text at all.
    assert!(parse(&[0xff, 0xfe, 0x00]).is_err());
}

/// A number is kept as the text it was. Nothing here needs its value, and
/// turning `1e1000` or a digest-sized integer into a float would silently
/// change what the manifest said.
#[test]
fn a_number_keeps_the_text_it_was_written_as() {
    let document = parse(br#"[1e1000, 18446744073709551616, 0.1]"#).expect("parse");
    let values = document.as_array().expect("array");
    assert_eq!(values[0], Value::Number("1e1000".to_string()));
    assert_eq!(values[1], Value::Number("18446744073709551616".to_string()));
    assert_eq!(values[2], Value::Number("0.1".to_string()));
}
