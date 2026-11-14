//! A Rust parser: parse generated source, walk the tree and print it back as tokens.
//!
//! The source is a handful of modules with structs, enums, impls, traits, functions and match
//! heavy bodies. A pass parses every module with `syn`, keeps the trees alive while a visitor
//! walks them, and then turns each tree back into a token stream and a string, which is what a
//! procedural macro does with its input.

use super::{
    Pass, Scale,
    text::{Rng, WORDS},
    twice,
};
use quote::ToTokens;
use std::{collections::HashMap, fmt::Write, time::Instant};
use syn::{File, Ident, Lit, visit::Visit};

const MODULES: usize = 6;

/// A type name that is the same on every call for one index.
fn type_name(i: usize) -> String {
    let word = WORDS[i % WORDS.len()];
    let mut name: String = word[..1].to_ascii_uppercase();
    name.push_str(&word[1..]);
    format!("{name}{i}")
}

fn push_statement(out: &mut String, rng: &mut Rng, k: usize) {
    let (n, m) = (rng.between(2, 900), rng.between(2, 40));
    let word = rng.pick(WORDS);
    match rng.below(12) {
        0 => {
            let _ = writeln!(
                out,
                "    let v{k}: Vec<u64> = (0..{n}).map(|x| x * {m} + {k}).filter(|x| x % 3 != 0).collect();"
            );
        }
        1 => {
            let _ = writeln!(
                out,
                "    let mut acc{k} = 0u64;\n    for (i, x) in input.iter().enumerate() {{\n        if *x > {n} {{ acc{k} += x; }} else if i % 2 == 0 {{ acc{k} ^= *x; }} else {{ continue; }}\n    }}"
            );
        }
        2 => {
            let _ = writeln!(
                out,
                "    let r{k} = match value % {m} {{\n        0 => 1,\n        1 | 2 => value.wrapping_mul(3),\n        n if n > {n} => n / 2,\n        _ => {{ let t = value + {k}; t * t }}\n    }};"
            );
        }
        3 => {
            let _ = writeln!(out, "    let s{k} = format!(\"{{}}-{{}}-{word}\", name, {k});");
        }
        4 => {
            let _ = writeln!(
                out,
                "    while let Some(top) = stack.pop() {{\n        stack.extend(top.children.iter().cloned());\n        count += {m};\n    }}"
            );
        }
        5 => {
            let _ = writeln!(
                out,
                "    let p{k}: Result<u32, String> = text.parse::<u32>().map_err(|e| e.to_string());\n    if let Ok(n) = p{k} {{ total += n as u64; }}"
            );
        }
        6 => {
            let _ = writeln!(
                out,
                "    let pt{k} = Point {{ x: 1.5, y: -2.0, label: String::from(\"{word}\"), ..Default::default() }};"
            );
        }
        7 => {
            let _ = writeln!(out, "    let add{k} = |a: i32, b: i32| -> i32 {{ a + b * {n} }};");
        }
        8 => {
            let _ = writeln!(
                out,
                "    let l{k} = loop {{\n        count += 1;\n        if count > {n} {{ break count * 2; }}\n    }};"
            );
        }
        9 => {
            let _ = writeln!(
                out,
                "    let (x{k}, y{k}, z{k}) = (value, name.len(), [1u8, 2, 3, {m}]);\n    let sl{k} = &z{k}[1..3];"
            );
        }
        10 => {
            let _ = writeln!(
                out,
                "    if total > {n} && !name.is_empty() {{\n        return Err(format!(\"bad {word} {{}}\", total));\n    }}"
            );
        }
        _ => {
            let _ = writeln!(
                out,
                "    let opt{k}: Option<&str> = Some(\"{word}\");\n    let len{k} = opt{k}.map(str::len).unwrap_or({m});"
            );
        }
    }
}

fn push_function(out: &mut String, rng: &mut Rng, index: usize) {
    let name = WORDS[index % WORDS.len()];
    let _ = writeln!(
        out,
        "pub fn {name}_{index}(value: u64, name: &str, text: &str, input: &[u64], stack: &mut Vec<Node>) -> Result<u64, String> {{\n    let mut total = 0u64;\n    let mut count = 0u64;"
    );
    for k in 0..rng.between(6, 16) {
        push_statement(out, rng, k);
    }
    out.push_str("    Ok(total + count + value)\n}\n\n");
}

fn push_struct(out: &mut String, rng: &mut Rng, index: usize) {
    let name = type_name(index);
    let other = type_name(index + 7);
    let _ = write!(
        out,
        "#[derive(Debug, Clone, PartialEq, Default)]\npub struct {name}<T: Clone> {{\n    pub id: u64,\n    pub name: String,\n    pub values: Vec<T>,\n    pub extra: Option<Box<{other}<u8>>>,\n"
    );
    for f in 0..rng.between(1, 5) {
        let _ = writeln!(
            out,
            "    pub {}_{f}: std::collections::HashMap<String, Vec<T>>,",
            rng.pick(WORDS)
        );
    }
    out.push_str("}\n\n");
    let _ = write!(
        out,
        "impl<T: Clone> {name}<T> {{\n    pub fn new(id: u64, name: impl Into<String>) -> Self {{\n        Self {{ id, name: name.into(), ..Default::default() }}\n    }}\n\n    pub fn get(&self, at: usize) -> Option<&T> {{\n        self.values.get(at)\n    }}\n\n    pub fn describe(&self) -> String {{\n        match (&self.extra, self.values.len()) {{\n            (Some(_), 0) => format!(\"{{}} with extra\", self.name),\n            (Some(e), n) => format!(\"{{}} with extra {{}} and {{}} values\", self.name, e.id, n),\n            (None, 0..=3) => self.name.clone(),\n            (None, n) if n > 100 => format!(\"{{}} big\", self.name),\n            (None, _) => format!(\"{{}} plain\", self.name),\n        }}\n    }}\n}}\n\n"
    );
}

fn push_enum(out: &mut String, rng: &mut Rng, index: usize) {
    let name = format!("Event{index}");
    let _ = writeln!(out, "#[derive(Debug, Clone)]\npub enum {name} {{\n    Idle,");
    for v in 0..rng.between(3, 8) {
        let word = rng.pick(WORDS);
        match rng.below(3) {
            0 => {
                let _ = writeln!(out, "    V{v}({}),", ["u32", "String", "Vec<u8>"][rng.below(3)]);
            }
            1 => {
                let _ = writeln!(out, "    V{v} {{ {word}: u64, label: String }},");
            }
            _ => {
                let _ = writeln!(out, "    V{v},");
            }
        }
    }
    let _ = write!(
        out,
        "}}\n\nimpl {name} {{\n    pub fn weight(&self) -> u32 {{\n        match self {{\n            Self::Idle => 0,\n            _ => 1,\n        }}\n    }}\n}}\n\n"
    );
}

fn push_trait(out: &mut String, index: usize) {
    let name = type_name(index);
    let _ = write!(
        out,
        "pub trait Handle{index} {{\n    type Output: Send + 'static;\n    const LIMIT: usize = {index};\n\n    fn handle(&mut self, input: &[u8]) -> Self::Output;\n\n    fn name(&self) -> &'static str {{\n        \"handler\"\n    }}\n}}\n\nimpl<T: Clone + Send + 'static> Handle{index} for {name}<T> {{\n    type Output = Vec<T>;\n\n    fn handle(&mut self, input: &[u8]) -> Self::Output {{\n        let mut out = Vec::with_capacity(input.len());\n        for byte in input {{\n            if let Some(value) = self.values.get(*byte as usize) {{\n                out.push(value.clone());\n            }}\n        }}\n        out\n    }}\n}}\n\n"
    );
}

fn module(items: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut out = String::new();
    out.push_str("use std::collections::HashMap;\n\npub struct Node {\n    pub children: Vec<Node>,\n}\n\n#[derive(Default)]\npub struct Point {\n    pub x: f64,\n    pub y: f64,\n    pub label: String,\n}\n\n");
    for index in 0..items {
        let at = seed as usize * 1000 + index;
        match index % 4 {
            0 => push_function(&mut out, &mut rng, at),
            1 => push_struct(&mut out, &mut rng, at),
            2 => push_enum(&mut out, &mut rng, at),
            _ => {
                push_struct(&mut out, &mut rng, at);
                push_trait(&mut out, at);
            }
        }
    }
    out
}

/// Counts what a tool would collect while walking a file.
#[derive(Default)]
struct Walker {
    nodes: usize,
    literals: usize,
    idents: HashMap<String, usize>,
}

impl<'ast> Visit<'ast> for Walker {
    fn visit_ident(&mut self, ident: &'ast Ident) {
        *self.idents.entry(ident.to_string()).or_default() += 1;
    }

    fn visit_expr(&mut self, expr: &'ast syn::Expr) {
        self.nodes += 1;
        syn::visit::visit_expr(self, expr);
    }

    fn visit_lit(&mut self, lit: &'ast Lit) {
        self.literals += 1;
        syn::visit::visit_lit(self, lit);
    }
}

fn pass(sources: &[String]) -> usize {
    let files: Vec<File> =
        sources.iter().map(|source| syn::parse_file(source).expect("valid source")).collect();
    let mut walker = Walker::default();
    for file in &files {
        walker.visit_file(file);
    }
    let printed: Vec<String> =
        files.iter().map(|file| file.to_token_stream().to_string()).collect();
    walker.nodes +
        walker.literals +
        walker.idents.len() +
        printed.iter().map(String::len).sum::<usize>()
}

/// One MB is a million bytes of source, counting every parse.
pub(crate) fn run(scale: Scale) -> [Pass; 2] {
    let sources: Vec<String> = (0..MODULES as u64).map(|seed| module(160, seed + 1)).collect();
    let rounds = scale.count(12);
    let mb = (sources.iter().map(String::len).sum::<usize>() * rounds) as f64 / 1e6;
    twice(|| {
        let start = Instant::now();
        let mut total = 0;
        for _ in 0..rounds {
            total += pass(&sources);
        }
        assert!(total > 0);
        Pass::since(start, mb)
    })
}
