//! The `nebula-stress` command.

use std::path::PathBuf;

use clap::Parser;
use console::style;
use nebula_stress::{Options, harness};

/// Drive a real Nebula editor over a real project, then run the programs in it.
#[derive(Debug, Parser)]
#[command(name = "nebula-stress", version, about, long_about = None)]
struct Cli {
    /// Where to write the fixture project.
    #[arg(long, default_value = "stress-results/fixture")]
    workdir: PathBuf,

    /// How many lines the generated file gets.
    #[arg(long, default_value_t = nebula_stress::fixture::GENERATED_LINES)]
    generated_lines: usize,

    /// How many keystrokes the editing phase delivers.
    #[arg(long, default_value_t = 5_000)]
    keystrokes: usize,

    /// Surface size, as WIDTHxHEIGHT.
    #[arg(long, default_value = "1920x1080")]
    surface: String,

    /// Never use the GPU, even if one is available.
    #[arg(long)]
    cpu: bool,

    /// Do not compile or run the fixture's programs.
    #[arg(long)]
    skip_programs: bool,

    /// Fail if any toolchain a program needs is missing, rather than skipping.
    #[arg(long)]
    require_all_toolchains: bool,

    /// Write the JSON report here.
    #[arg(long, default_value = "stress-results/report.json")]
    json: PathBuf,

    /// Write the Markdown report here.
    #[arg(long, default_value = "stress-results/REPORT.md")]
    markdown: PathBuf,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NEBULA_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .without_time()
        .init();

    if let Err(error) = run(Cli::parse()) {
        eprintln!("{} {error:#}", style("error:").red().bold());
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let (width, height) = cli
        .surface
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("`{}` is not a size; write it as 1920x1080", cli.surface))?;

    let options = Options {
        workdir: cli.workdir,
        generated_lines: cli.generated_lines,
        keystrokes: cli.keystrokes,
        surface: (width.trim().parse()?, height.trim().parse()?),
        force_cpu: cli.cpu,
        skip_programs: cli.skip_programs,
        require_all_toolchains: cli.require_all_toolchains,
    };

    println!("{}", style("Nebula end-to-end stress run").bold());
    println!();
    for (tool, present) in harness::available_toolchains() {
        println!(
            "  {:<8} {}",
            tool,
            if present {
                style("available").green().to_string()
            } else {
                style("missing (that program will be skipped)").yellow().to_string()
            }
        );
    }
    println!();

    let report = nebula_stress::run(&options)?;

    for phase in &report.phases {
        let verdict = if phase.skipped {
            style("skipped").dim().to_string()
        } else if phase.passed() {
            style("ok").green().to_string()
        } else {
            style("FAILED").red().bold().to_string()
        };
        println!("{:<14} {verdict:<20} {:.2?}", phase.name, phase.elapsed);

        for measurement in &phase.measurements {
            let marker = match measurement.budget {
                Some(_) if !measurement.within_budget() => style("  over budget").red().to_string(),
                Some(budget) => style(format!("  (budget {budget:.0?})")).dim().to_string(),
                None => String::new(),
            };
            println!("    {:<28} {:>10.2?}{marker}", measurement.name, measurement.elapsed);
        }
        for note in &phase.notes {
            println!("    {}", style(note).dim());
        }
        for failure in &phase.failures {
            println!("    {} {failure}", style("×").red());
        }
    }

    if !report.programs.is_empty() {
        println!();
        println!("{}", style("Programs").bold());
        for program in &report.programs {
            let verdict = if program.skipped {
                style("skipped").dim().to_string()
            } else if program.passed {
                style("ok").green().to_string()
            } else {
                style("FAILED").red().bold().to_string()
            };
            println!("  {:<12} {verdict:<20} {}", program.language, program.output.lines().next().unwrap_or(""));
        }
    }

    write(&cli.json, &serde_json::to_string_pretty(&report)?)?;
    write(&cli.markdown, &report.to_markdown())?;

    println!();
    println!("  report  {}", cli.json.display());
    println!("  summary {}", cli.markdown.display());
    println!();

    if report.passed {
        println!("{} in {:.2?}", style("✓ every phase passed").green().bold(), report.elapsed);
        Ok(())
    } else {
        for failure in report.failures() {
            eprintln!("  {} {failure}", style("×").red());
        }
        anyhow::bail!("the stress run failed")
    }
}

fn write(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}
