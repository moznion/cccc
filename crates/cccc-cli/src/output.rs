//! Output rendering: JSON (default) and the human-readable table / ranking
//! views. The report *data* and aggregation live in `cccc_core::report`; this
//! module only formats them.

use std::io::{IsTerminal, Write};

use cccc_core::report::{FunctionReport, MetricSummary, Report, Summary, TopReport};
use serde::Serialize;

/// True when ANSI color should be used on `stream`: it is a real terminal and
/// the user hasn't opted out via `NO_COLOR` (<https://no-color.org>). Piped or
/// redirected output stays plain so logs and CI captures aren't polluted with
/// escape codes.
fn use_color(stream: &impl IsTerminal) -> bool {
    std::env::var_os("NO_COLOR").is_none() && stream.is_terminal()
}

/// Wrap `msg` in ANSI yellow when `enabled`.
fn yellow(msg: &str, enabled: bool) -> std::borrow::Cow<'_, str> {
    if enabled {
        format!("\x1b[33m{msg}\x1b[0m").into()
    } else {
        msg.into()
    }
}

/// Print a warning line to stderr, in yellow when stderr is a terminal.
pub fn warn(msg: &str) {
    eprintln!("{}", yellow(msg, use_color(&std::io::stderr())));
}

/// Print any serializable value as pretty JSON to stdout.
///
/// Serializes straight into a buffered, locked stdout writer rather than
/// building one big `String` first — for large reports (zod's corpus is ~1 MB
/// of JSON) that avoids materializing the whole document in memory.
pub fn print_json<T: Serialize>(value: &T) {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let result = serde_json::to_writer_pretty(&mut out, value)
        .map_err(std::io::Error::from)
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    if let Err(e) = result {
        eprintln!("cccc: failed to write JSON: {e}");
    }
}

/// Print a human-readable table to stdout. Within each level functions are
/// sorted by cognitive complexity (desc); nested functions are indented. A
/// project summary is printed last.
///
/// All rows go through one buffered, locked stdout writer; `writeln!` to a
/// `BufWriter` never fails for stdout in practice, so write errors (e.g. a
/// closed pipe) are reported once at the end rather than per line.
pub fn print_table(report: &Report) {
    let color = use_color(&std::io::stdout());
    with_stdout(|out| {
        for file in &report.files {
            writeln!(out, "{}", file.path)?;
            writeln!(out, "  {:>9}  {:>10}  Function", "Cognitive", "Cyclomatic")?;
            if file.functions.is_empty() {
                writeln!(out, "  (no functions)")?;
            }
            for f in sorted_desc(&file.functions) {
                write_fn(out, f, 1)?;
            }
            writeln!(out, "  {}", "-".repeat(48))?;
            writeln!(
                out,
                "  file total: cognitive={} cyclomatic={}",
                file.cognitive, file.cyclomatic
            )?;
            for e in &file.parse_errors {
                writeln!(out, "{}", yellow(&format!("  parse warning: {e}"), color))?;
            }
            writeln!(out)?;
        }
        write_summary(out, &report.summary, color)
    });
}

/// Print a flat ranking as a human-readable table, followed by the summary.
pub fn print_top_table(report: &TopReport) {
    let color = use_color(&std::io::stdout());
    with_stdout(|out| {
        writeln!(out, "top {} by {}", report.top.len(), report.metric)?;
        writeln!(out, "  {:>9}  {:>10}  Function", "Cognitive", "Cyclomatic")?;
        if report.top.is_empty() {
            writeln!(out, "  (no functions)")?;
        }
        for e in &report.top {
            writeln!(
                out,
                "  {:>9}  {:>10}  {} [{}] {}:{}",
                e.cognitive, e.cyclomatic, e.name, e.kind, e.path, e.line
            )?;
        }
        writeln!(out, "  {}", "-".repeat(48))?;
        write_summary(out, &report.summary, color)
    });
}

/// Run `body` against a buffered, locked stdout writer and report any I/O error
/// once. Centralizes the locking/flushing all table printers share.
fn with_stdout<F>(body: F)
where
    F: FnOnce(&mut dyn Write) -> std::io::Result<()>,
{
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    if let Err(e) = body(&mut out).and_then(|()| out.flush()) {
        eprintln!("cccc: failed to write output: {e}");
    }
}

fn write_summary(out: &mut dyn Write, s: &Summary, color: bool) -> std::io::Result<()> {
    writeln!(
        out,
        "summary ({} files, {} functions)",
        s.file_count, s.function_count
    )?;
    writeln!(
        out,
        "  {:<11} {:>5} {:>5} {:>7} {:>5} {:>5}",
        "", "sum", "max", "median", "p90", "p95"
    )?;
    write_metric_row(out, "cognitive", &s.cognitive)?;
    write_metric_row(out, "cyclomatic", &s.cyclomatic)?;
    if s.parse_error_count > 0 {
        let line = format!(
            "  parse errors: {} in {} file(s)",
            s.parse_error_count, s.parse_error_file_count
        );
        writeln!(out, "{}", yellow(&line, color))?;
    }
    Ok(())
}

fn write_metric_row(out: &mut dyn Write, label: &str, m: &MetricSummary) -> std::io::Result<()> {
    writeln!(
        out,
        "  {label:<11} {:>5} {:>5} {:>7} {:>5} {:>5}",
        m.sum, m.max, m.median, m.p90, m.p95
    )
}

/// Borrow each function in display order (cognitive desc, then line asc) without
/// cloning the tree.
fn sorted_desc(fns: &[FunctionReport]) -> Vec<&FunctionReport> {
    let mut refs: Vec<&FunctionReport> = fns.iter().collect();
    refs.sort_by(|a, b| b.cognitive.cmp(&a.cognitive).then(a.line.cmp(&b.line)));
    refs
}

fn write_fn(out: &mut dyn Write, f: &FunctionReport, depth: usize) -> std::io::Result<()> {
    let indent = "  ".repeat(depth);
    writeln!(
        out,
        "  {:>9}  {:>10}  {indent}{} [{}] (L{})",
        f.cognitive, f.cyclomatic, f.name, f.kind, f.line
    )?;
    for c in sorted_desc(&f.children) {
        write_fn(out, c, depth + 1)?;
    }
    Ok(())
}
