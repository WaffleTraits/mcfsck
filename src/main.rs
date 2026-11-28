//! mcfsck: find invalid and corrupt chunks in Minecraft worlds, and remove
//! them safely.

mod codec;
mod fix;
mod nbt;
mod progress;
mod repair;
mod report;
mod scan;
#[cfg(test)]
mod testutil;
mod tui;
mod util;
mod world;

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use fix::{FixOptions, Status};
use progress::Eta;
use scan::{Finding, Fix, Kind, ScanOutput, Severity, Stats};
use std::io::{stderr, stdin, stdout, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::Duration;
use util::{human_bytes, human_count, s};

const EXAMPLES: &str = "\
Examples:
  mcfsck tui  /srv/minecraft/world            full-screen: scan, browse, preview, fix
  mcfsck scan /srv/minecraft/world            list every problem (exit code 1 if any)
  mcfsck scan world --quick                   headers only, no decompression
  mcfsck scan world -o report.jsonl           also save a machine-readable report
  mcfsck fix  world                           dry run: show what would be removed
  mcfsck fix  world --apply                   remove unloadable chunks (asks first)
  mcfsck fix  world --repair                  dry run: show what can be repaired without loss
  mcfsck fix  world --repair --apply          repair that; nothing is removed

WORLD can be a world folder, a dimension folder, a region folder or a single
r.X.Z.mca file. Scanning never writes. Stop the server before `fix --apply`;
changed files are first copied to WORLD/mcfsck-backups/<time>/.";

#[derive(Parser)]
#[command(name = "mcfsck", version, about, after_help = EXAMPLES, max_term_width = 100)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check every chunk and list the problems (read-only)
    Scan(ScanArgs),
    /// Remove chunks the game cannot load so that it generates them anew, or with --repair
    /// repair those that can be repaired without loss (dry run unless --apply)
    Fix(FixArgs),
    /// Full-screen interface: scan, browse, preview and fix
    Tui(TuiArgs),
}

#[derive(Args, Clone)]
struct Common {
    /// World folder, dimension folder, region folder or one r.X.Z.mca file
    world: PathBuf,
    /// Only check the region and chunk headers; do not decompress chunk data (much faster, finds less)
    #[arg(long)]
    quick: bool,
    /// Worker threads [default: all cores]
    #[arg(short = 'j', long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=256))]
    threads: Option<u16>,
}

impl Common {
    fn options(&self) -> scan::Options {
        scan::Options { quick: self.quick, threads: self.threads.map(usize::from) }
    }
}

#[derive(Args)]
struct ScanArgs {
    #[command(flatten)]
    common: Common,
    /// Also save the findings as JSON lines to this file
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
    /// Print JSON lines instead of text
    #[arg(long)]
    json: bool,
    /// Do not list warnings
    #[arg(long)]
    errors_only: bool,
}

#[derive(Args, Clone)]
struct Safety {
    /// Do not copy files to WORLD/mcfsck-backups before changing them
    #[arg(long)]
    no_backup: bool,
    /// Write even if the world seems to be open in a game or server
    #[arg(long)]
    force: bool,
    /// When removing a terrain chunk, keep its entity and point-of-interest data
    #[arg(long)]
    keep_linked: bool,
}

#[derive(Args)]
struct FixArgs {
    #[command(flatten)]
    common: Common,
    /// Really make the changes (without this, fix only shows what it would do)
    #[arg(long)]
    apply: bool,
    /// Show what would be done and write nothing (the default)
    #[arg(long, conflicts_with = "apply")]
    dry_run: bool,
    /// Repair instead of remove: put right what can be put right without losing anything (bad
    /// length fields, compression markers and sector counts, lost header entries, misfiled
    /// chunks). Removes nothing; what cannot be repaired is listed and left as it is
    #[arg(long, conflicts_with = "warnings")]
    repair: bool,
    /// Also fix warnings: unused .mcc files, and chunks that are odd but still load
    #[arg(long)]
    warnings: bool,
    /// Only fix these kinds of problems (comma-separated codes as printed by `scan --json`, e.g. corrupt,bad_nbt)
    #[arg(long, value_name = "KIND", value_delimiter = ',', value_parser = parse_kind)]
    kind: Vec<Kind>,
    /// With --apply: do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
    #[command(flatten)]
    safety: Safety,
}

#[derive(Args)]
struct TuiArgs {
    #[command(flatten)]
    common: Common,
    /// Where `w` saves the report
    #[arg(short, long, value_name = "FILE", default_value = "mcfsck-report.jsonl")]
    output: PathBuf,
    /// Start without mouse support (toggle with `m`)
    #[arg(long)]
    no_mouse: bool,
    /// Colour scheme (switch with `t`)
    #[arg(long, value_enum, default_value_t)]
    theme: tui::ThemeName,
    #[command(flatten)]
    safety: Safety,
}

fn parse_kind(text: &str) -> Result<Kind, String> {
    Kind::ALL.into_iter().find(|k| k.code() == text.trim()).ok_or_else(|| {
        let all: Vec<&str> = Kind::ALL.iter().map(|k| k.code()).collect();
        format!("unknown kind; the kinds are: {}", all.join(", "))
    })
}

/// Run the scan with a progress line on stderr (if that is a terminal).
fn scan_with_progress(common: &Common) -> Result<ScanOutput> {
    let stats = Stats::default();
    let opts = common.options();
    let show = stderr().is_terminal();
    let result = std::thread::scope(|scope| {
        let worker = scope.spawn(|| scan::run(&common.world, &opts, &stats));
        let mut eta = Eta::new();
        while !worker.is_finished() {
            std::thread::sleep(Duration::from_millis(100));
            if !show {
                continue;
            }
            let load = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
            let (done, total) = (load(&stats.bytes_done), load(&stats.total_bytes));
            eta.update(done);
            let problems = load(&stats.errors) + load(&stats.warnings);
            let line = if total == 0 {
                "Looking for region files...".to_string()
            } else {
                format!(
                    "{:3.0}%  {}/{} files  {}/{}  {}/s  left {}  {} problem{}",
                    done as f64 * 100.0 / total as f64,
                    human_count(load(&stats.files_done)),
                    human_count(load(&stats.total_files)),
                    human_bytes(done),
                    human_bytes(total),
                    human_bytes(eta.rate() as u64),
                    eta.text(total),
                    human_count(problems),
                    s(problems),
                )
            };
            eprint!("\r{line:<78}");
            let _ = stderr().flush();
        }
        worker.join()
    });
    if show {
        eprint!("\r{:<78}\r", "");
        let _ = stderr().flush();
    }
    match result {
        Ok(result) => result,
        Err(_) => bail!("the scan thread panicked"),
    }
}

/// A scan of a world that is being played can catch chunks half-written.
fn note_if_in_use(out: &ScanOutput) {
    if !out.findings.is_empty() && world::in_use(&out.root) {
        eprintln!(
            "note: this world is open in a running game or server. Chunks it was saving during the scan can look damaged; scan again with it stopped to be sure."
        );
    }
}

fn cmd_scan(args: &ScanArgs) -> Result<ExitCode> {
    let out = scan_with_progress(&args.common)?;
    note_if_in_use(&out);
    if let Some(path) = &args.output {
        report::save_jsonl(path, &out)?;
    }
    let mut stdout = stdout().lock();
    if args.json {
        report::write_jsonl(&mut stdout, &out)?;
    } else {
        for f in &out.findings {
            if !(args.errors_only && f.severity() == Severity::Warning) {
                writeln!(stdout, "{}", f.line())?;
            }
        }
        if !out.findings.is_empty() {
            writeln!(stdout)?;
        }
        for line in out.coverage_lines() {
            writeln!(stdout, "{line}")?;
        }
        writeln!(stdout, "{}", out.verdict())?;
        if let Some(path) = &args.output {
            writeln!(stdout, "Report saved to {}", path.display())?;
        }
        if out.count(Severity::Error) > 0 {
            let world = args.common.world.display();
            writeln!(stdout, "`mcfsck fix {world} --repair` shows what can be repaired without losing anything.")?;
            writeln!(stdout, "`mcfsck fix {world}` shows what removing the unloadable chunks would do.")?;
        }
    }
    Ok(if out.findings.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

fn confirm(question: &str) -> Result<bool> {
    if !stdin().is_terminal() {
        bail!("not asking for confirmation without a terminal; add --yes to apply without asking");
    }
    print!("{question} [y/N] ");
    stdout().flush()?;
    let mut answer = String::new();
    stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// `fix --repair`: repair what can be repaired without loss; remove nothing.
fn cmd_repair(args: &FixArgs, out: &ScanOutput) -> Result<ExitCode> {
    let picked: Vec<&Finding> = out
        .findings
        .iter()
        .filter(|f| if args.kind.is_empty() { f.severity() == Severity::Error } else { args.kind.contains(&f.kind) })
        .collect();
    if picked.is_empty() {
        println!("Nothing to repair{}.", if out.findings.is_empty() { "" } else { ": there are no errors of that kind" });
        return Ok(ExitCode::SUCCESS);
    }
    let mut opts = FixOptions { dry_run: true, backup: !args.safety.no_backup, force: args.safety.force, linked: false };
    let progress = fix::Progress::default();
    let preview = repair::execute(&out.root, &picked, &opts, &progress)?;
    println!();
    for (o, f) in preview.outcomes.iter().zip(&picked) {
        println!("{}", o.line(f, true));
    }
    for w in &preview.warnings {
        println!("warning: {w}");
    }
    println!();
    println!("{}", preview.summary());
    let (repairable, hopeless, _) = preview.repair_counts();
    let rest = |n: u64| {
        if n > 0 {
            println!(
                "What cannot be repaired can only be removed, so that the game generates it anew: `mcfsck fix {}` shows that.",
                args.common.world.display()
            );
        }
    };
    if repairable == 0 || !args.apply {
        if repairable > 0 {
            println!("Run again with --apply to make these repairs.");
        }
        rest(hopeless);
        return Ok(ExitCode::SUCCESS);
    }
    if !opts.force && world::in_use(&out.root) {
        bail!("this world is open in a running game or server. Stop it first; nothing was written.");
    }
    let question = format!(
        "Repair {} chunk{} in {}{}?",
        human_count(repairable),
        s(repairable),
        out.root.display(),
        if opts.backup { "" } else { " WITHOUT a backup" }
    );
    if !args.yes && !confirm(&question)? {
        println!("Nothing was written.");
        return Ok(ExitCode::SUCCESS);
    }
    opts.dry_run = false;
    let done = repair::execute(&out.root, &picked, &opts, &progress)?;
    println!();
    for (o, f) in done.outcomes.iter().zip(&picked) {
        if matches!(o.status, Status::Skipped(_)) {
            println!("{}", o.line(f, false));
        }
    }
    for w in &done.warnings {
        println!("warning: {w}");
    }
    println!("{}", done.summary());
    if let Some(dir) = &done.backup_dir {
        println!("The original of every changed file is in {}", dir.display());
        println!("To undo, copy those files back over the world's (with the server stopped).");
    }
    rest(done.repair_counts().1);
    Ok(ExitCode::SUCCESS)
}

fn cmd_fix(args: &FixArgs) -> Result<ExitCode> {
    let out = scan_with_progress(&args.common)?;
    note_if_in_use(&out);
    println!("{}", out.speed_line());
    println!("{}", out.verdict());
    if args.repair {
        return cmd_repair(args, &out);
    }
    let picked: Vec<&Finding> = out
        .findings
        .iter()
        .filter(|f| f.fix() != Fix::None)
        .filter(|f| {
            if args.kind.is_empty() {
                args.warnings || f.severity() == Severity::Error
            } else {
                args.kind.contains(&f.kind)
            }
        })
        .collect();
    let unpicked = out.findings.len() - picked.len();
    if picked.is_empty() {
        println!("Nothing to fix{}.", if unpicked > 0 { " with these options (see `mcfsck scan` for what was found)" } else { "" });
        return Ok(ExitCode::SUCCESS);
    }

    let mut opts = FixOptions {
        dry_run: true,
        backup: !args.safety.no_backup,
        force: args.safety.force,
        linked: !args.safety.keep_linked,
    };
    let progress = fix::Progress::default();
    let preview = fix::execute(&out.root, &picked, &opts, &progress)?;
    println!();
    for (o, f) in preview.outcomes.iter().zip(&picked) {
        println!("{}", o.line(f, true));
    }
    for w in &preview.warnings {
        println!("warning: {w}");
    }
    println!();
    println!("{}", preview.summary());
    if unpicked > 0 {
        println!(
            "{} other finding{} not touched (warnings need --warnings; some have no fix).",
            human_count(unpicked as u64),
            if unpicked == 1 { " is" } else { "s are" }
        );
    }
    let (chunks, files, _, _) = preview.counts();
    if !args.apply {
        if chunks + files > 0 {
            println!("Run again with --apply to make these changes.");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if chunks + files == 0 {
        println!("Nothing can be applied.");
        return Ok(ExitCode::SUCCESS);
    }
    if !opts.force && world::in_use(&out.root) {
        bail!("this world is open in a running game or server. Stop it first; nothing was written.");
    }
    let question = format!(
        "Remove {} chunk{} and {} file{} from {}{}?",
        human_count(chunks),
        s(chunks),
        human_count(files),
        s(files),
        out.root.display(),
        if opts.backup { "" } else { " WITHOUT a backup" }
    );
    if !args.yes && !confirm(&question)? {
        println!("Nothing was written.");
        return Ok(ExitCode::SUCCESS);
    }

    opts.dry_run = false;
    let done = fix::execute(&out.root, &picked, &opts, &progress)?;
    println!();
    for (o, f) in done.outcomes.iter().zip(&picked) {
        if matches!(o.status, Status::Skipped(_)) {
            println!("{}", o.line(f, false));
        }
    }
    for w in &done.warnings {
        println!("warning: {w}");
    }
    println!("{}", done.summary());
    if let Some(dir) = &done.backup_dir {
        println!("The original of every changed file is in {}", dir.display());
        println!("To undo, copy those files back over the world's (with the server stopped).");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_tui(args: &TuiArgs) -> Result<ExitCode> {
    tui::run(tui::Config {
        world: args.common.world.clone(),
        scan: args.common.options(),
        report_path: args.output.clone(),
        mouse: !args.no_mouse,
        theme: args.theme,
        backup: !args.safety.no_backup,
        force: args.safety.force,
        linked: !args.safety.keep_linked,
    })?;
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.cmd {
        Cmd::Scan(args) => cmd_scan(args),
        Cmd::Fix(args) => cmd_fix(args),
        Cmd::Tui(args) => cmd_tui(args),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses() {
        let cli = Cli::try_parse_from(["mcfsck", "fix", "w", "--kind", "corrupt,bad_nbt", "-j", "4", "--apply", "-y"]).unwrap();
        let Cmd::Fix(args) = cli.cmd else { panic!() };
        assert_eq!(args.kind, [Kind::Corrupt, Kind::BadNbt]);
        assert_eq!(args.common.threads, Some(4));
        assert!(args.apply && args.yes && !args.safety.no_backup);
        assert!(Cli::try_parse_from(["mcfsck", "fix", "w", "--apply", "--dry-run"]).is_err());
        let cli = Cli::try_parse_from(["mcfsck", "fix", "w", "--repair", "--apply"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Fix(args) if args.repair && args.apply));
        assert!(Cli::try_parse_from(["mcfsck", "fix", "w", "--repair", "--warnings"]).is_err());
        assert!(Cli::try_parse_from(["mcfsck", "fix", "w", "--kind", "nonsense"]).is_err());
        assert!(Cli::try_parse_from(["mcfsck", "scan", "w", "-j", "0"]).is_err());
        assert!(Cli::try_parse_from(["mcfsck", "tui", "w", "--theme", "mc", "--no-mouse"]).is_ok());
    }
}
