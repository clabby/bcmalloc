//! The figure of `assets/benchmarks.svg`: for every run and every other allocator, one dot per
//! workload for how much faster or slower bcmalloc is.

use crate::{
    markdown::{geomean, machine_name, ratio, suite_name},
    model::{Data, Run},
};
use std::fmt::Write;

/// The allocator the figure is about.
const SUBJECT: &str = "bcmalloc";

/// The colours, light and dark, that the figure gives to the other allocators, in this order.
const COLORS: [(&str, &str, &str); 5] = [
    ("mimalloc", "#8250df", "#bc8cff"),
    ("snmalloc", "#1a7f37", "#3fb950"),
    ("jemalloc", "#bc4c00", "#f0883e"),
    ("tcmalloc", "#9a6700", "#d29922"),
    ("system", "#6e7781", "#8b949e"),
];

const WIDTH: f64 = 980.0;
const LEFT: f64 = 240.0;
const RIGHT: f64 = 760.0;
/// The range of the axis, as powers of two: from a quarter to sixteen times.
const LOW: f64 = -2.0;
const HIGH: f64 = 4.0;
const ROW: f64 = 24.0;
const SECTION: f64 = 34.0;

fn x_of(ratio: f64) -> f64 {
    let at = ratio.log2().clamp(LOW, HIGH);
    LEFT + (at - LOW) / (HIGH - LOW) * (RIGHT - LEFT)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// How bcmalloc's speed compares with `other`'s on every speed workload of a run, above 1 when
/// bcmalloc is faster.
pub fn ratios(run: &Run, other: &str) -> Vec<f64> {
    run.workloads
        .iter()
        .filter(|w| w.is_speed())
        .filter_map(|w| {
            Some(
                ratio(run, w, &w.primary, SUBJECT, false)? /
                    ratio(run, w, &w.primary, other, false)?,
            )
        })
        .collect()
}

/// The other allocators of a run, in the order of [`COLORS`].
fn others(run: &Run) -> Vec<&str> {
    let mut others: Vec<&str> =
        run.allocators.iter().map(String::as_str).filter(|a| *a != SUBJECT).collect();
    others.sort_by_key(|a| COLORS.iter().position(|(name, ..)| name == a).unwrap_or(COLORS.len()));
    others
}

fn class(allocator: &str) -> String {
    format!("c-{}", allocator.replace(|c: char| !c.is_ascii_alphanumeric(), ""))
}

/// Renders the figure.
pub fn render(data: &Data) -> String {
    let runs: Vec<&Run> =
        data.runs.iter().filter(|run| run.allocators.iter().any(|a| a == SUBJECT)).collect();
    let rows: usize = runs.iter().map(|run| others(run).len()).sum();
    let top = 118.0;
    let height = top + runs.len() as f64 * SECTION + rows as f64 * ROW + 56.0;

    let mut light = String::new();
    let mut dark = String::new();
    for (name, color, night) in COLORS {
        let _ = write!(light, ".{}{{fill:{color};stroke:{color}}}", class(name));
        let _ = write!(dark, ".{}{{fill:{night};stroke:{night}}}", class(name));
    }
    let mut s = String::new();
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {height}" width="{WIDTH}" height="{height}" role="img" aria-label="For every suite and every other allocator, one dot per workload showing how much faster bcmalloc is"><title>How bcmalloc compares, workload by workload</title><style>
:root{{--ink:#1f2328;--muted:#59636e;--line:#d1d9e0;--page:#ffffff;--panel:#f6f8fa}}
@media (prefers-color-scheme: dark){{:root{{--ink:#e6edf3;--muted:#9198a1;--line:#3d444d;--page:#0d1117;--panel:#151b23}}{dark}}}
text{{font-family:ui-sans-serif, -apple-system, "Segoe UI", Helvetica, Arial, sans-serif;fill:var(--ink);font-size:13px}}
.title{{font-size:15px;font-weight:600}}.muted{{fill:var(--muted)}}.small{{font-size:11.5px}}.b{{font-weight:600}}
.h{{font-size:12px;font-weight:600;letter-spacing:.04em;text-transform:uppercase;fill:var(--muted)}}
.grid{{stroke:var(--line);stroke-width:1}}.even{{stroke:var(--muted);stroke-width:1.5;stroke-dasharray:4 4}}
.dot{{fill-opacity:.55;stroke:none}}.mean{{stroke-width:3;fill:none}}
{light}
</style><rect x="0" y="0" width="{WIDTH}" height="{height}" style="fill:var(--page)"/>"#
    );
    let _ = write!(
        s,
        r#"<text class="title" x="40" y="34">How bcmalloc compares, workload by workload</text>"#
    );
    let _ = write!(
        s,
        r#"<text class="small muted" x="40" y="54">Each dot is one workload: bcmalloc's speed divided by the other allocator's, on a log scale. Right of the dashed line,</text><text class="small muted" x="40" y="70">bcmalloc is faster. The bar is the geometric mean. Medians of every run, from benchmarks.json.</text>"#
    );
    // The axis, at the top and through every row.
    let body_top = top - 10.0;
    let body_bottom = height - 46.0;
    for power in (LOW as i32)..=(HIGH as i32) {
        let ratio = 2_f64.powi(power);
        let x = x_of(ratio);
        let class = if power == 0 { "even" } else { "grid" };
        let _ =
            write!(s, r#"<path class="{class}" d="M{x:.1} {body_top} L{x:.1} {body_bottom}"/>"#);
        let label = match power {
            -2 => "4× slower".to_string(),
            -1 => "2× slower".to_string(),
            0 => "even".to_string(),
            p => format!("{}× faster", 1 << p),
        };
        let _ = write!(
            s,
            r#"<text class="small muted" x="{x:.1}" y="{:.1}" text-anchor="middle">{label}</text>"#,
            top - 18.0
        );
    }
    let mut y = top;
    for run in &runs {
        let title = format!("{}, {}", suite_name(&run.suite), machine_name(&run.machine));
        let mut chars = title.chars();
        let title: String =
            chars.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default() +
                chars.as_str();
        let count = run.workloads.iter().filter(|w| w.is_speed()).count();
        let _ =
            write!(s, r#"<text class="h" x="40" y="{:.1}">{}</text>"#, y + 20.0, escape(&title));
        let _ = write!(
            s,
            r#"<text class="small muted" x="{RIGHT:.1}" y="{:.1}" text-anchor="end">{count} workloads</text>"#,
            y + 20.0
        );
        y += SECTION;
        for other in others(run) {
            let ratios = ratios(run, other);
            let mid = y + ROW / 2.0;
            let _ =
                write!(s, r#"<text x="56" y="{:.1}">against {}</text>"#, mid + 4.0, escape(other));
            let _ = write!(
                s,
                r#"<path class="grid" d="M{LEFT} {mid:.1} L{RIGHT} {mid:.1}" style="stroke-opacity:.5"/>"#
            );
            for (index, ratio) in ratios.iter().enumerate() {
                // A little vertical spread keeps dots of equal ratios apart.
                let jitter = ((index * 7) % 9) as f64 - 4.0;
                let _ = write!(
                    s,
                    r#"<circle class="dot {}" cx="{:.1}" cy="{:.1}" r="3.4"/>"#,
                    class(other),
                    x_of(*ratio),
                    mid + jitter
                );
            }
            if let Some(mean) = geomean(ratios.iter().copied()) {
                let x = x_of(mean);
                let _ = write!(
                    s,
                    r#"<path class="mean {}" d="M{x:.1} {:.1} L{x:.1} {:.1}"/>"#,
                    class(other),
                    mid - 9.0,
                    mid + 9.0
                );
                let faster = ratios.iter().filter(|r| **r > 1.0).count();
                let _ = write!(
                    s,
                    r#"<text class="small" x="{:.1}" y="{:.1}"><tspan class="b">{mean:.2}×</tspan><tspan class="muted"> faster on {faster} of {}</tspan></text>"#,
                    RIGHT + 16.0,
                    mid + 4.0,
                    ratios.len()
                );
            }
            y += ROW;
        }
    }
    let _ = write!(
        s,
        r#"<text class="small muted" x="40" y="{:.1}">Dots beyond the ends of the axis sit at the ends. Workloads that measure memory are left out, and so is the latency group.</text>"#,
        height - 20.0
    );
    s += "</svg>\n";
    s
}
