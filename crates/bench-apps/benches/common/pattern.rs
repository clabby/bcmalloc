//! A regex engine over generated text.
//!
//! The text is a mix of log lines, access log lines, prose and key value records. A pass compiles
//! a few dozen patterns, then searches the whole text with most of them, runs others on every line
//! with captures or replacement, and collects owned strings from the matches the way a log tool
//! does.

use super::{
    Pass, Scale,
    text::{Rng, WORDS, capitalized, push_words},
    twice,
};
use regex::{Regex, RegexSet};
use std::{collections::HashMap, fmt::Write, time::Instant};

const LEVELS: [&str; 5] = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];
const MODULES: [&str; 6] =
    ["net::tcp", "db::pool", "http::server", "cache::lru", "auth::token", "queue::worker"];
const METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];
const FAILURES: [&str; 5] =
    ["connection refused", "timeout", "reset by peer", "unreachable", "failed"];

/// Patterns that search the whole text and keep every match.
const WHOLE: [&str; 24] = [
    r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b",
    r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
    r#"https?://[^\s"<>]+"#,
    r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b",
    r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z\b",
    r"\b(?:ERROR|WARN|INFO|DEBUG|TRACE)\b",
    r"(?i)\bfail(?:ed|ure|s)?\b",
    r"\b[A-Z][a-z]+(?:\s[A-Z][a-z]+)+\b",
    r"\b0x[0-9a-fA-F]+\b",
    r"\$\d+(?:\.\d{2})?",
    r#""[^"\\]*(?:\\.[^"\\]*)*""#,
    r"\b\w+_id=\d+\b",
    r"\b(?:GET|POST|PUT|DELETE|PATCH) /[^\s?]*",
    r"\b\d+ms\b",
    r"(?i)timeout|refused|reset by peer|unreachable",
    r"\b[a-z]+(?:::[a-z_]+)+\b",
    r"\bv\d+\.\d+\.\d+\b",
    r"\b\d+x\d+\b",
    r"\p{Lu}\p{Ll}+",
    r"\b\w{12,}\b",
    r"(?m)^\S+ - user=\w+",
    r"\b[a-z]+=\d{5}\b",
    r"\.(?:com|org|so)\b",
    r"\s{2,}",
];

/// Patterns that run on every line and read their capture groups.
const CAPTURE: [&str; 4] = [
    r"^(?P<ts>\S+) (?P<level>[A-Z]+) \[(?P<module>[\w:]+)\] (?P<msg>.*)$",
    r"user_id=(?P<user>\d+) session_id=(?P<session>\d+)",
    r#"(?P<method>GET|POST|PUT|DELETE|PATCH) (?P<path>/\S*) HTTP/1\.1" (?P<status>\d{3}) (?P<bytes>\d+)"#,
    r"(?P<key>\w+)=(?P<value>\d+)",
];

/// Patterns that rewrite every line.
const REPLACE: [&str; 2] =
    [r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b", r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"];

/// Patterns that are tried together on every line.
const SET: [&str; 6] =
    [r"ERROR", r"WARN", r"timeout", r"refused", r#"HTTP/1\.1" 5\d\d"#, r"charged"];

fn push_ip(out: &mut String, rng: &mut Rng) {
    let (a, b, c, d) = (rng.below(256), rng.below(256), rng.below(256), rng.below(256));
    let _ = write!(out, "{a}.{b}.{c}.{d}");
}

fn push_uuid(out: &mut String, rng: &mut Rng) {
    let _ = write!(
        out,
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        rng.next() as u32,
        rng.next() as u16,
        rng.next() as u16,
        rng.next() as u16,
        rng.next() & 0xffff_ffff_ffff,
    );
}

fn push_line(out: &mut String, rng: &mut Rng) {
    match rng.below(10) {
        // A log line.
        0..=3 => {
            let _ = write!(
                out,
                "2026-{:02}-{:02}T{:02}:{:02}:{:02}Z {} [{}] ",
                rng.between(1, 12),
                rng.between(1, 28),
                rng.below(24),
                rng.below(60),
                rng.below(60),
                rng.pick(&LEVELS),
                rng.pick(&MODULES),
            );
            match rng.below(4) {
                0 => {
                    out.push_str("request ");
                    push_uuid(out, rng);
                    let _ = write!(out, " took {}ms", rng.below(900));
                }
                1 => {
                    out.push_str("peer ");
                    push_ip(out, rng);
                    let _ = write!(out, " {} after {} retries", rng.pick(&FAILURES), rng.below(9));
                }
                2 => {
                    let _ = write!(
                        out,
                        "user_id={} session_id={} charged ${}.{:02} at 0x{:x}",
                        rng.below(100_000),
                        rng.below(1_000_000),
                        rng.below(500),
                        rng.below(100),
                        rng.next() as u32,
                    );
                }
                _ => {
                    let _ = write!(
                        out,
                        "loaded v{}.{}.{} from \"/srv/{}/{}.so\" ({}x{})",
                        rng.below(5),
                        rng.below(20),
                        rng.below(50),
                        rng.pick(WORDS),
                        rng.pick(WORDS),
                        rng.between(64, 4096),
                        rng.between(64, 4096),
                    );
                }
            }
        }
        // An access log line.
        4..=6 => {
            push_ip(out, rng);
            let _ = write!(
                out,
                " - user={} [{}/Nov/2026] \"{} /{}/{}?id={} HTTP/1.1\" {} {} \"http://{}.example.com/{}\"",
                rng.pick(WORDS),
                rng.between(1, 28),
                rng.pick(&METHODS),
                rng.pick(WORDS),
                rng.pick(WORDS),
                rng.below(10_000),
                rng.pick(&[200, 200, 200, 204, 301, 404, 500]),
                rng.below(100_000),
                rng.pick(WORDS),
                rng.pick(WORDS),
            );
        }
        // Prose with names, an address and a failure now and then.
        7 | 8 => {
            for _ in 0..rng.between(1, 3) {
                let _ = write!(out, "{} {} met ", capitalized(rng), capitalized(rng));
                let words = rng.between(4, 12);
                push_words(out, rng, words);
                let _ = write!(out, ".  Mail {}@{}.org. ", rng.pick(WORDS), rng.pick(WORDS));
            }
        }
        // A key value record.
        _ => {
            for _ in 0..rng.between(3, 8) {
                let _ = write!(out, "{}={} ", rng.pick(WORDS), rng.below(100_000));
            }
            push_uuid(out, rng);
        }
    }
    out.push('\n');
}

fn generate(bytes: usize) -> String {
    let mut rng = Rng::new(0x5245_4745);
    let mut out = String::with_capacity(bytes + 1024);
    while out.len() < bytes {
        push_line(&mut out, &mut rng);
    }
    out
}

fn compile(patterns: &[&str]) -> Vec<Regex> {
    patterns.iter().map(|pattern| Regex::new(pattern).expect("valid pattern")).collect()
}

/// Compiles every pattern and searches the text. Returns a figure that depends on every result, so
/// that the work cannot be skipped.
fn pass(text: &str) -> usize {
    let whole = compile(&WHOLE);
    let capture = compile(&CAPTURE);
    let replace = compile(&REPLACE);
    let set = RegexSet::new(SET).expect("valid set");
    let mut total = 0;

    for regex in &whole {
        let matches: Vec<String> = regex.find_iter(text).map(|m| m.as_str().to_owned()).collect();
        total += matches.len();
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    for regex in &capture {
        for line in text.lines() {
            if let Some(found) = regex.captures(line) {
                let key = found.get(1).map_or("", |group| group.as_str()).to_owned();
                *counts.entry(key).or_default() += 1;
            }
        }
    }
    total += counts.len();

    for regex in &replace {
        let rewritten: Vec<String> =
            text.lines().map(|line| regex.replace_all(line, "<redacted>").into_owned()).collect();
        total += rewritten.iter().map(String::len).sum::<usize>();
    }

    total += text.lines().map(|line| set.matches(line).iter().count()).sum::<usize>();
    total
}

/// One MB is a million bytes of text per pattern, and a set of patterns counts as one. A round
/// compiles every pattern again, as a program that builds its regexes per task would.
pub(crate) fn run(scale: Scale) -> [Pass; 2] {
    let text = generate(3_000_000);
    let rounds = scale.count(8);
    let patterns = WHOLE.len() + CAPTURE.len() + REPLACE.len() + 1;
    let mb = (text.len() * patterns * rounds) as f64 / 1e6;
    twice(|| {
        let start = Instant::now();
        let mut total = 0;
        for _ in 0..rounds {
            total += pass(&text);
        }
        assert!(total > 0);
        Pass::since(start, mb)
    })
}
