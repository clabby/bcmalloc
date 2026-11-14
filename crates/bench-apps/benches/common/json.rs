//! A JSON pipeline: parse a large document, rebuild parts of it, and write it out again.
//!
//! The document is a list of records with nested objects, arrays and strings of varied length. One
//! iteration parses it into a dynamic `Value`, rebuilds maps and filters arrays, serializes the
//! result, and then does the same through derived structs. Like a service that reshapes documents,
//! it keeps the previous iteration's output alive while it builds the next one.

use super::{
    Pass, Scale,
    text::{Rng, WORDS, capitalized, push_words},
    twice,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{fmt::Write, time::Instant};

#[derive(Serialize, Deserialize)]
struct Geo {
    lat: f64,
    lon: f64,
}

#[derive(Serialize, Deserialize)]
struct Address {
    street: String,
    city: String,
    zip: String,
    geo: Geo,
}

#[derive(Serialize, Deserialize)]
struct Event {
    at: u64,
    event: String,
    delta: i64,
}

#[derive(Serialize, Deserialize)]
struct Record {
    id: u64,
    name: String,
    active: bool,
    score: f64,
    email: String,
    tags: Vec<String>,
    address: Address,
    history: Vec<Event>,
    bio: String,
    scores: Vec<f64>,
}

/// Writes one record. Floats have three decimals so that they survive a round trip unchanged.
fn push_record(out: &mut String, rng: &mut Rng, id: usize) {
    let name = format!("{} {}", capitalized(rng), capitalized(rng));
    let _ = write!(
        out,
        r#"{{"id":{id},"name":"{name}","active":{},"score":{}.{:03},"email":"{}@{}.example.com","tags":["#,
        rng.below(3) != 0,
        rng.below(1000),
        rng.below(1000),
        rng.pick(WORDS),
        rng.pick(WORDS),
    );
    for i in 0..rng.between(0, 9) {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", rng.pick(WORDS));
    }
    let _ = write!(
        out,
        r#"],"address":{{"street":"{} {} street","city":"{}","zip":"{:05}","geo":{{"lat":{}.{:03},"lon":-{}.{:03}}}}},"history":["#,
        rng.below(900) + 1,
        rng.pick(WORDS),
        capitalized(rng),
        rng.below(100_000),
        rng.below(90),
        rng.below(1000),
        rng.below(180),
        rng.below(1000),
    );
    for i in 0..rng.between(0, 12) {
        if i > 0 {
            out.push(',');
        }
        let delta = rng.below(200) as i64 - 100;
        let _ = write!(
            out,
            r#"{{"at":{},"event":"{}","delta":{delta}}}"#,
            1_600_000_000 + rng.below(100_000_000),
            rng.pick(WORDS),
        );
    }
    out.push_str(r#"],"bio":""#);
    let words = rng.between(3, 90);
    push_words(out, rng, words);
    // A few strings carry escapes, which take a slower path in the parser.
    if rng.below(8) == 0 {
        out.push_str("\\n\\\"quoted\\\" caf\\u00e9");
    }
    out.push_str(r#"","scores":["#);
    for i in 0..rng.between(0, 16) {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}.{:03}", rng.below(30), rng.below(1000));
    }
    out.push_str("]}");
}

fn document(records: usize) -> String {
    let mut rng = Rng::new(0x4a53_4f4e);
    let mut out = String::with_capacity(records * 1200);
    out.push('[');
    for id in 0..records {
        if id > 0 {
            out.push(',');
        }
        push_record(&mut out, &mut rng, id);
    }
    out.push(']');
    out
}

/// Rebuilds one record as a new object, with renamed keys, filtered arrays and a derived field.
fn reshape(record: Value) -> Value {
    let Value::Object(fields) = record else { return record };
    let mut out = Map::new();
    let mut city = String::new();
    for (key, value) in fields {
        match (key.as_str(), value) {
            ("name", Value::String(name)) => {
                out.insert("display_name".to_string(), Value::String(name.to_uppercase()));
            }
            ("tags", Value::Array(tags)) => {
                let kept = tags
                    .into_iter()
                    .filter(|tag| tag.as_str().is_some_and(|tag| tag.len() > 4))
                    .map(|tag| json!({ "label": tag, "weight": 1 }))
                    .collect();
                out.insert("tags".to_string(), Value::Array(kept));
            }
            ("address", Value::Object(mut address)) => {
                city = address.get("city").and_then(Value::as_str).unwrap_or("").to_string();
                address.remove("geo");
                out.insert("address".to_string(), Value::Object(address));
            }
            ("history", Value::Array(events)) => {
                let kept = events
                    .iter()
                    .filter(|event| event["delta"].as_i64().is_some_and(|delta| delta >= 0))
                    .map(|event| {
                        let mut copy = Map::new();
                        for (key, value) in event.as_object().into_iter().flatten() {
                            copy.insert(key.clone(), value.clone());
                        }
                        copy.insert("kind".to_string(), json!(event["event"].to_string()));
                        Value::Object(copy)
                    })
                    .collect();
                out.insert("history".to_string(), Value::Array(kept));
            }
            ("bio", Value::String(bio)) => {
                let short: String = bio.chars().take(120).collect();
                out.insert("bio".to_string(), Value::String(short));
            }
            ("scores", Value::Array(scores)) => {
                let kept = scores
                    .into_iter()
                    .filter(|score| score.as_f64().is_some_and(|score| score > 10.0))
                    .collect();
                out.insert("scores".to_string(), Value::Array(kept));
            }
            (_, value) => {
                out.insert(key, value);
            }
        }
    }
    let summary = format!("{} in {city}", out["id"]);
    out.insert("summary".to_string(), Value::String(summary));
    Value::Object(out)
}

/// What an iteration leaves behind, so that the next one starts while it is still alive.
struct Output {
    tree: Value,
    text: String,
    typed: String,
}

fn iteration(input: &str) -> Output {
    let Value::Array(records) = serde_json::from_str(input).expect("valid document") else {
        panic!("the document is an array");
    };
    let tree = Value::Array(records.into_iter().map(reshape).collect());
    let text = serde_json::to_string(&tree).expect("serializable");

    let mut records: Vec<Record> = serde_json::from_str(input).expect("valid records");
    records.retain(|record| record.active);
    for record in &mut records {
        record.tags.sort();
        record.tags.dedup();
        record.history.retain(|event| event.delta < 0);
    }
    let typed = serde_json::to_string_pretty(&records).expect("serializable");
    Output { tree, text, typed }
}

/// One MB is a million bytes of input, counting every parse.
pub(crate) fn run(scale: Scale) -> [Pass; 2] {
    let input = document(2_500);
    let iterations = scale.count(32);
    let mb = (input.len() * 2 * iterations) as f64 / 1e6;
    twice(|| {
        let start = Instant::now();
        let mut kept = None;
        for _ in 0..iterations {
            // The old output is dropped only after the new one exists.
            let output = iteration(&input);
            kept = Some(output);
        }
        let kept = kept.expect("at least one iteration");
        assert!(kept.tree.is_array() && !kept.text.is_empty() && !kept.typed.is_empty());
        Pass::since(start, mb)
    })
}
