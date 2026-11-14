//! The documents rendered from the results: the generated parts of `BENCHMARKS.md` and
//! `README.md`, and `assets/benchmarks.svg`.
//!
//! A generated part of a Markdown file sits between `<!-- generated:NAME -->` and
//! `<!-- /generated:NAME -->`, and everything outside those markers is written by hand.

use crate::{
    markdown::{machine_name, machines, mimalloc_bench, overview, report, summary},
    model::Data,
    svg,
};
use std::{fs, io, path::Path};

/// The part of a document called `name`, for the results in `data`.
fn part(data: &Data, name: &str) -> Option<String> {
    match name {
        "overview" => Some(overview(data)),
        "machines" => Some(machines(data)),
        _ => {
            let (suite, rest) = name.split_once(':')?;
            let (machine, view) = rest.split_once(':').unwrap_or((rest, "report"));
            let run = data.run(suite, machine)?;
            Some(match (suite, view) {
                ("mimalloc-bench", _) => mimalloc_bench(run),
                (_, "summary") => summary(run),
                (_, _) => format!(
                    "<details>\n<summary>Every workload on {}</summary>\n\n{}\n</details>\n",
                    machine_name(machine),
                    report(run)
                ),
            })
        }
    }
}

/// Replaces every generated part of `text`.
pub fn fill(data: &Data, text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<!-- generated:") {
        let open_end = start + rest[start..].find("-->").map_or(rest.len() - start, |at| at + 3);
        let name = rest[start + "<!-- generated:".len()..open_end - 3].trim().to_string();
        let close = format!("<!-- /generated:{name} -->");
        let Some(close_at) = rest[open_end..].find(&close).map(|at| at + open_end) else { break };
        out += &rest[..open_end];
        out += "\n";
        out += &part(data, &name).unwrap_or_else(|| "No results yet.\n".to_string());
        out += &close;
        rest = &rest[close_at + close.len()..];
    }
    out + rest
}

/// The files that are rendered from the results, with their new contents.
pub fn render(root: &Path, data: &Data) -> io::Result<Vec<(std::path::PathBuf, String)>> {
    let mut files = Vec::new();
    for name in ["BENCHMARKS.md", "README.md"] {
        let path = root.join(name);
        files.push((path.clone(), fill(data, &fs::read_to_string(&path)?)));
    }
    files.push((root.join("assets/benchmarks.svg"), svg::render(data)));
    Ok(files)
}

/// Writes every rendered file that changed.
pub fn write(root: &Path, data: &Data) -> io::Result<()> {
    for (path, text) in render(root, data)? {
        if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            fs::write(&path, text)?;
        }
    }
    Ok(())
}

/// The rendered files whose contents differ from what the results give.
pub fn stale(root: &Path, data: &Data) -> io::Result<Vec<std::path::PathBuf>> {
    Ok(render(root, data)?
        .into_iter()
        .filter(|(path, text)| fs::read_to_string(path).ok().as_deref() != Some(text.as_str()))
        .map(|(path, _)| path)
        .collect())
}
