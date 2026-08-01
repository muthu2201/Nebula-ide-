//! The `nebula` command.
//!
//! With no subcommand it opens a window. Every other subcommand is headless, so
//! the same binary is usable over SSH, in a container and in CI — which is how
//! the stress harness drives a real editor with no display server.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use console::style;
use nebula_ide::{App, Config, VERSION, session::Session};

/// An AI-native, GPU-accelerated code editor.
#[derive(Debug, Parser)]
#[command(name = "nebula", version = VERSION, about, long_about = None)]
struct Cli {
    /// Files or a directory to open.
    paths: Vec<PathBuf>,

    /// Use a specific config file instead of the default location.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Never use the GPU, even if one is available.
    #[arg(long, global = true)]
    cpu: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Replay a session script against a real editor and report timings.
    Run {
        /// The script to replay.
        script: PathBuf,
        /// The directory relative paths in the script resolve against.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Write the timing report as JSON here.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Fail if the 95th-percentile frame time exceeds this, in milliseconds.
        #[arg(long)]
        budget_ms: Option<u64>,
    },

    /// Render a file to a PNG without opening a window.
    Render {
        /// The file to render.
        file: PathBuf,
        /// Where to write the image.
        #[arg(long, short)]
        out: PathBuf,
        /// Surface size in pixels.
        #[arg(long, default_value = "1280x800")]
        size: String,
        /// Scroll to this 1-based line first.
        #[arg(long)]
        line: Option<usize>,
    },

    /// Report what this machine supports and what the editor found.
    Doctor,

    /// Print the effective configuration, including every default.
    Config {
        /// Write the defaults to the config file instead of printing them.
        #[arg(long)]
        write: bool,
    },

    /// Check a licence file.
    License {
        /// The licence to check. Defaults to the one in the config directory.
        file: Option<PathBuf>,
    },
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
    let (mut config, warning) = match &cli.config {
        Some(path) => Config::load_from(path),
        None => Config::load(),
    };
    if let Some(warning) = warning {
        eprintln!("{} {warning}", style("warning:").yellow().bold());
    }
    if cli.cpu {
        config.force_cpu_renderer = true;
    }

    match cli.command {
        Some(Command::Run { script, root, report, budget_ms }) => {
            run_script(config, &script, &root, report.as_deref(), budget_ms)
        }
        Some(Command::Render { file, out, size, line }) => render(config, &file, &out, &size, line),
        Some(Command::Doctor) => doctor(config),
        Some(Command::Config { write }) => print_config(&config, write, cli.config.as_deref()),
        Some(Command::License { file }) => check_license(file.as_deref()),
        None => open_editor(config, &cli.paths),
    }
}

/// Open the window.
fn open_editor(config: Config, paths: &[PathBuf]) -> anyhow::Result<()> {
    #[cfg(not(feature = "gui"))]
    {
        let _ = (config, paths);
        anyhow::bail!(
            "this build has no window support. \
             Rebuild with `--features gui`, or use `nebula run`, `nebula render` or `nebula doctor`."
        );
    }

    #[cfg(feature = "gui")]
    {
        let width = config.window[0] as u32;
        let height = config.window[1] as u32;

        // A single directory argument means "open this project"; anything else
        // is a list of files.
        let mut app = match paths {
            [only] if only.is_dir() => App::open_project(config, only, width, height)?,
            _ => App::new(config, width, height)?,
        };

        for path in paths {
            if path.is_dir() {
                continue;
            }
            if let Err(error) = app.open(path) {
                eprintln!("{} {error}", style("warning:").yellow().bold());
            }
        }

        nebula_ide::window::run(app)?;
        Ok(())
    }
}

/// Replay a script and report what it cost.
fn run_script(
    mut config: Config,
    script: &std::path::Path,
    root: &std::path::Path,
    report_path: Option<&std::path::Path>,
    budget_ms: Option<u64>,
) -> anyhow::Result<()> {
    let session = Session::load(script)?;
    let width = config.window[0] as u32;
    let height = config.window[1] as u32;
    config.window = [width as f32, height as f32];

    let mut app = App::new(config, width, height)?;
    println!(
        "{} {} on the {} renderer",
        style("Running").green().bold(),
        script.display(),
        app.renderer_kind().name()
    );

    let report = session.run(&mut app, root)?;

    println!();
    println!("  steps        {}", report.steps);
    println!("  keystrokes   {}", report.keystrokes);
    println!("  frames       {}", report.frames);
    println!("  elapsed      {:.2?}", report.elapsed);

    if let Some(p50) = report.frame_p50() {
        println!("  frame p50    {p50:.2?}");
    }
    if let Some(p95) = report.frame_p95() {
        println!("  frame p95    {p95:.2?}");
    }
    if let Some(worst) = report.worst_frame() {
        println!("  frame worst  {worst:.2?}");
    }
    if let Some(latency) = report.latency_p95() {
        println!("  latency p95  {latency:.2?}");
    }

    for error in &report.errors {
        println!("  {} {error}", style("error:").red());
    }

    if let Some(path) = report_path {
        let json = serde_json::json!({
            "script": script.display().to_string(),
            "renderer": app.renderer_kind().name(),
            "steps": report.steps,
            "keystrokes": report.keystrokes,
            "frames": report.frames,
            "elapsed_ms": report.elapsed.as_secs_f64() * 1000.0,
            "frame_p50_ms": report.frame_p50().map(|d| d.as_secs_f64() * 1000.0),
            "frame_p95_ms": report.frame_p95().map(|d| d.as_secs_f64() * 1000.0),
            "frame_worst_ms": report.worst_frame().map(|d| d.as_secs_f64() * 1000.0),
            "latency_p95_ms": report.latency_p95().map(|d| d.as_secs_f64() * 1000.0),
            "errors": report.errors,
        });
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&json)?)?;
        println!();
        println!("  report written to {}", path.display());
    }

    if !report.errors.is_empty() {
        anyhow::bail!("the session reported {} error(s)", report.errors.len());
    }

    // The budget is checked last so that the numbers are printed either way —
    // a CI failure that hides its own measurements is not much use.
    let budget = budget_ms.map(std::time::Duration::from_millis);
    if let Some(budget) = budget
        && !report.within_budget(budget)
    {
        anyhow::bail!(
            "the 95th-percentile frame took {:.2?}, over the {budget:.2?} budget",
            report.frame_p95().unwrap_or_default()
        );
    }

    println!();
    println!("{} {}", style("✓").green().bold(), style("session completed").bold());
    Ok(())
}

/// Rasterise a file and write it out as a PNG.
fn render(
    config: Config,
    file: &std::path::Path,
    out: &std::path::Path,
    size: &str,
    line: Option<usize>,
) -> anyhow::Result<()> {
    let (width, height) = parse_size(size)?;

    let mut app = App::new(config, width, height)?;
    app.open(file)?;

    if let Some(line) = line {
        app.view.viewport.first_line = line.saturating_sub(1);
    }

    let framebuffer = app.frame(1.0)?;
    write_png(out, &framebuffer)?;

    println!(
        "{} {} ({}x{}, {} renderer, {:.2?})",
        style("Wrote").green().bold(),
        out.display(),
        framebuffer.width,
        framebuffer.height,
        app.renderer_kind().name(),
        app.last_frame_time().unwrap_or_default()
    );
    Ok(())
}

/// `WIDTHxHEIGHT`.
fn parse_size(size: &str) -> anyhow::Result<(u32, u32)> {
    let (width, height) = size
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("`{size}` is not a size; write it as 1280x800"))?;
    Ok((width.trim().parse()?, height.trim().parse()?))
}

/// Write an RGBA framebuffer as a PNG.
///
/// Hand-rolled rather than pulled from a crate: the encoder needs to produce
/// exactly one kind of image, and a stored-mode zlib stream is a few dozen lines
/// that nothing else in the tree has to depend on.
fn write_png(
    path: &std::path::Path,
    framebuffer: &nebula_render::backend::Framebuffer,
) -> anyhow::Result<()> {
    use std::io::Write;

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in bytes {
            crc ^= *byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_input = Vec::with_capacity(4 + data.len());
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    }

    let (width, height) = (framebuffer.width, framebuffer.height);

    // Each scanline is prefixed with filter type 0 (none).
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for y in 0..height {
        raw.push(0);
        let start = (y as usize) * (width as usize) * 4;
        raw.extend_from_slice(&framebuffer.pixels[start..start + width as usize * 4]);
    }

    // A zlib stream of stored (uncompressed) deflate blocks: valid, portable,
    // and large — which is fine for a screenshot written once.
    let mut zlib = vec![0x78, 0x01];
    for (index, block) in raw.chunks(65_535).enumerate() {
        let last = (index + 1) * 65_535 >= raw.len();
        zlib.push(if last { 1 } else { 0 });
        zlib.extend_from_slice(&(block.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    // Adler-32 over the uncompressed data.
    let (mut a, mut b): (u32, u32) = (1, 0);
    for byte in &raw {
        a = (a + *byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlacing.
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(&png)?;
    Ok(())
}

/// Report what this machine can do.
fn doctor(config: Config) -> anyhow::Result<()> {
    println!("{} {VERSION}", style("Nebula").bold());
    println!();

    println!("{}", style("Renderer").bold());
    match App::new(config.clone(), 640, 480) {
        Ok(app) => {
            println!("  backend      {}", app.renderer_kind().name());
            println!("  reason       {}", app.renderer_reason());
            println!("  frame budget {:?}", app.renderer_kind().frame_budget());
        }
        Err(error) => println!("  {} {error}", style("unavailable:").red()),
    }
    println!();

    println!("{}", style("Sandbox").bold());
    println!("  backend      {}", nebula_sandbox::backend_description());
    println!("  filesystem   {}", yes_no(nebula_sandbox::is_available()));
    if !nebula_sandbox::is_available() {
        println!(
            "  {} extensions and tool calls will run with fewer restrictions than the",
            style("note:").yellow()
        );
        println!("        design assumes. Everything else works unchanged.");
    }
    println!();

    println!("{}", style("Languages").bold());
    let languages = nebula_ide::Workspace::supported_languages();
    println!("  grammars     {} ({})", languages.len(), languages.join(", "));
    println!();

    println!("{}", style("Model access").bold());
    println!("  default      {}", config.model);
    let keys = nebula_ai::KeyStore::os();
    for provider in nebula_ai::Provider::ALL {
        println!(
            "  {:<12} {}",
            provider.id(),
            if keys.has(*provider) { "key available" } else { "no key stored" }
        );
    }
    println!();

    println!("{}", style("Configuration").bold());
    println!("  directory    {}", Config::directory().display());
    println!("  file         {}", Config::path().display());
    println!(
        "  exists       {}",
        if Config::path().exists() { "yes" } else { "no (defaults in use)" }
    );

    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "enforced" } else { "unavailable on this kernel" }
}

/// Print or write the effective configuration.
fn print_config(
    config: &Config,
    write: bool,
    explicit: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    if !write {
        println!("{}", serde_json::to_string_pretty(config)?);
        return Ok(());
    }

    let path = explicit.map(PathBuf::from).unwrap_or_else(Config::path);
    if path.exists() {
        anyhow::bail!(
            "{} already exists; delete it first if you meant to replace it",
            path.display()
        );
    }
    config.save_to(&path)?;
    println!("{} {}", style("Wrote").green().bold(), path.display());
    Ok(())
}

/// Validate a licence file against this machine.
fn check_license(file: Option<&std::path::Path>) -> anyhow::Result<()> {
    let path = file.map(PathBuf::from).unwrap_or_else(|| Config::directory().join("license.json"));

    if !path.exists() {
        println!("{} no licence at {}", style("Unlicensed:").yellow().bold(), path.display());
        println!("The editor runs unlicensed with the AI features disabled.");
        return Ok(());
    }

    let validator = nebula_license::Validator::production()?;
    let machine = nebula_license::Fingerprint::collect();
    let status = validator.validate_file(&path, &machine)?;

    println!("{} {}", style("Licence").bold(), path.display());
    println!("  machine      {}", machine.short_id());
    match status.message() {
        Some(message) => println!("  status       {message}"),
        None => println!("  status       valid"),
    }

    if !status.is_usable() {
        anyhow::bail!("this licence is not usable on this machine");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("1280x800").unwrap(), (1280, 800));
        assert_eq!(parse_size("640X480").unwrap(), (640, 480));
        assert!(parse_size("huge").is_err());
        assert!(parse_size("1280x").is_err());
    }

    #[test]
    fn the_cli_parses_its_own_help() {
        // clap panics at runtime on a malformed command definition, so building
        // the parser is itself the assertion.
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn a_rendered_png_has_a_valid_header_and_decodes_to_the_right_size() {
        let dir = tempfile::TempDir::new().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(&source, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();

        let config = Config { force_cpu_renderer: true, ..Config::default() };

        let out = dir.path().join("shot.png");
        render(config, &source, &out, "320x240", None).unwrap();

        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");

        // IHDR sits immediately after the signature and the 4-byte length.
        assert_eq!(&bytes[12..16], b"IHDR");
        let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
        assert_eq!((width, height), (320, 240));
        assert_eq!(bytes[24], 8, "8 bits per channel");
        assert_eq!(bytes[25], 6, "RGBA");

        assert!(bytes.ends_with(b"IEND\xae\x42\x60\x82"), "the file is truncated");
    }

    #[test]
    fn a_rendered_png_is_not_a_blank_image() {
        // A PNG with a valid header and nothing in it would pass every
        // structural check above.
        let dir = tempfile::TempDir::new().unwrap();
        let source = dir.path().join("a.rs");
        std::fs::write(&source, "fn main() {}").unwrap();

        let config = Config { force_cpu_renderer: true, ..Config::default() };
        let mut app = App::new(config, 200, 100).unwrap();
        app.open(&source).unwrap();

        let frame = app.frame(1.0).unwrap();
        let distinct: std::collections::HashSet<[u8; 4]> =
            frame.pixels.chunks_exact(4).map(|p| [p[0], p[1], p[2], p[3]]).collect();
        assert!(distinct.len() > 1, "every pixel is the same colour");
    }

    #[test]
    fn a_session_script_runs_from_the_command_line_path() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        let script = dir.path().join("edit.nbs");
        std::fs::write(&script, "open main.rs\nend\ntype // trailing\nsave\nexpect-saved\nframe\n")
            .unwrap();

        let config = Config { force_cpu_renderer: true, ..Config::default() };
        run_script(config, &script, dir.path(), None, None).unwrap();

        let written = std::fs::read_to_string(dir.path().join("main.rs")).unwrap();
        assert!(written.contains("// trailing"), "{written}");
    }

    #[test]
    fn a_session_over_budget_fails_the_command() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let script = dir.path().join("s.nbs");
        std::fs::write(&script, "open a.txt\nframe\n").unwrap();

        let config = Config { force_cpu_renderer: true, ..Config::default() };
        // A zero-millisecond budget is one no real frame can meet, which is
        // exactly what makes it a test of the check rather than of the machine.
        let error = run_script(config, &script, dir.path(), None, Some(0)).unwrap_err();
        assert!(error.to_string().contains("budget"), "{error}");
    }

    #[test]
    fn a_run_report_is_written_as_json() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let script = dir.path().join("s.nbs");
        std::fs::write(&script, "open a.txt\ntype yz\nframe\n").unwrap();

        let config = Config { force_cpu_renderer: true, ..Config::default() };
        let report = dir.path().join("out/report.json");
        run_script(config, &script, dir.path(), Some(&report), None).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
        assert_eq!(json["keystrokes"], 2);
        assert!(json["frame_p95_ms"].as_f64().unwrap() >= 0.0);
    }

    #[test]
    fn doctor_reports_without_failing_on_a_machine_with_no_gpu() {
        let config = Config { force_cpu_renderer: true, ..Config::default() };
        doctor(config).unwrap();
    }

    #[test]
    fn writing_a_config_refuses_to_clobber_an_existing_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let config = Config::default();

        print_config(&config, true, Some(&path)).unwrap();
        assert!(path.exists());

        let error = print_config(&config, true, Some(&path)).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
    }

    #[test]
    fn a_missing_licence_is_reported_rather_than_fatal() {
        let dir = tempfile::TempDir::new().unwrap();
        check_license(Some(&dir.path().join("nothing.json"))).unwrap();
    }
}
