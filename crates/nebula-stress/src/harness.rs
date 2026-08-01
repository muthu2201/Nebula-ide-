//! The stress run.
//!
//! Six phases, each measuring one claim the blueprint makes:
//!
//! 1. **Cold start** — an editor is constructed and draws its first frame.
//! 2. **Editing** — thousands of keystrokes go through the real keymap into a
//!    real document, with a frame drawn after each one.
//! 3. **Large file** — the same, on a 200 000-line file, to show that per-frame
//!    cost tracks the window rather than the document.
//! 4. **Indexing** — the repo map, the content search and the vector index are
//!    built over the generated project.
//! 5. **Programs** — every fixture program is compiled and executed under the
//!    sandbox, and its output is checked.
//! 6. **Durability** — everything edited is saved and read back byte for byte.
//!
//! Nothing here is simulated. Phase 5 in particular runs `rustc`, `cc`, `go`,
//! `python3`, `node` and `sh` as real child processes and fails if what they
//! print is not what the fixture says it should be.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nebula_ide::{App, Config};
use nebula_ui::input::{Action, Key, KeyEvent, Motion};
use serde::{Deserialize, Serialize};

use crate::fixture::{Fixture, Program};
use crate::report::{PhaseReport, ProgramRun, Report, Timings};

/// How the run is configured.
#[derive(Debug, Clone)]
pub struct Options {
    /// Where to write the fixture.
    pub workdir: std::path::PathBuf,
    /// How many lines the generated file gets.
    pub generated_lines: usize,
    /// How many keystrokes the editing phase delivers.
    pub keystrokes: usize,
    /// Surface size, in pixels.
    pub surface: (u32, u32),
    /// Force the CPU renderer even where a GPU exists.
    pub force_cpu: bool,
    /// Skip the phase that compiles and runs the fixture's programs.
    pub skip_programs: bool,
    /// Fail if a toolchain a program needs is missing, rather than skipping it.
    pub require_all_toolchains: bool,
    /// Whether a missed timing budget fails the run.
    ///
    /// Correctness is always gated. Timings are not always meaningful: a shared
    /// CI runner measures the queue as much as the editor, so there the numbers
    /// are worth recording without being worth failing on.
    pub enforce_budgets: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            workdir: std::env::temp_dir().join("nebula-stress"),
            generated_lines: crate::fixture::GENERATED_LINES,
            keystrokes: 5_000,
            surface: (1920, 1080),
            force_cpu: false,
            skip_programs: false,
            require_all_toolchains: false,
            enforce_budgets: true,
        }
    }
}

/// The performance budgets the run is measured against.
///
/// Taken from the blueprint, not invented here: a budget the harness picks for
/// itself is a budget that always passes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Budgets {
    /// Keystroke to finished frame, on the GPU path.
    pub keystroke_to_photon_gpu: Duration,
    /// Keystroke to finished frame, on the CPU path.
    pub keystroke_to_photon_cpu: Duration,
    /// Constructing an editor and drawing the first frame.
    pub cold_start: Duration,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            keystroke_to_photon_gpu: nebula_render::budget::KEYSTROKE_TO_PHOTON_GPU,
            keystroke_to_photon_cpu: nebula_render::budget::KEYSTROKE_TO_PHOTON_CPU,
            cold_start: nebula_render::budget::COLD_START,
        }
    }
}

impl Budgets {
    /// The frame budget for the renderer that is actually drawing.
    ///
    /// A GPU adapter that is itself a software rasteriser — lavapipe on a CI
    /// runner, llvmpipe in a VM — is held to the software budget. Holding it to
    /// the hardware one would measure the emulator.
    pub fn frame(&self, hardware_accelerated: bool) -> Duration {
        if hardware_accelerated {
            self.keystroke_to_photon_gpu
        } else {
            self.keystroke_to_photon_cpu
        }
    }
}

/// Run every phase.
pub fn run(options: &Options) -> Result<Report> {
    let started = Instant::now();
    let budgets = Budgets::default();

    let fixture = Fixture::generate(&options.workdir, options.generated_lines)
        .context("could not write the fixture project")?;

    let config = Config { force_cpu_renderer: options.force_cpu, ..Config::default() };

    let mut report = Report::new(&fixture, budgets);

    let (mut app, cold) = cold_start(&config, options)?;
    report.renderer = app.renderer_kind().name().to_string();
    report.renderer_reason = app.renderer_reason().to_string();
    report.hardware_accelerated = app.is_hardware_accelerated();
    report.budgets_enforced = options.enforce_budgets;
    report.phases.push(cold);

    report.phases.push(recover("editing", editing(&mut app, &fixture, options, budgets)));
    report.phases.push(recover("large-file", large_file(&mut app, &fixture, options, budgets)));
    report.phases.push(recover("indexing", indexing(&fixture)));

    if options.skip_programs {
        report.phases.push(PhaseReport::skipped("programs", "--skip-programs was given"));
    } else {
        match programs(&fixture, options) {
            Ok((phase, runs)) => {
                report.programs = runs;
                report.phases.push(phase);
            }
            Err(error) => report.phases.push(failed_phase("programs", &error)),
        }
    }

    report.phases.push(recover("durability", durability(&mut app, &fixture)));

    report.elapsed = started.elapsed();
    report.finish();
    Ok(report)
}

/// Turn a phase that returned an error into a phase that reports it.
///
/// The alternative — propagating — loses every measurement taken before the
/// failure and writes no report at all, which is the opposite of what a harness
/// is for.
fn recover(name: &str, result: Result<PhaseReport>) -> PhaseReport {
    match result {
        Ok(phase) => phase,
        Err(error) => failed_phase(name, &error),
    }
}

fn failed_phase(name: &str, error: &anyhow::Error) -> PhaseReport {
    let mut phase = PhaseReport::new(name);
    phase.fail(format!("{error:#}"));
    phase.finish(Duration::ZERO);
    phase
}

/// Phase 1: how long it takes to have something on screen.
fn cold_start(config: &Config, options: &Options) -> Result<(App, PhaseReport)> {
    let started = Instant::now();
    let mut app = App::new(config.clone(), options.surface.0, options.surface.1)
        .context("the editor would not start")?;
    app.frame(1.0).context("the first frame failed")?;
    let elapsed = started.elapsed();

    let budget = Budgets::default().cold_start;
    let mut phase = PhaseReport::new("cold-start");
    phase.note(format!("{} renderer", app.renderer_kind().name()));
    phase.measure("time-to-first-frame", elapsed, Some(budget));
    phase.finish(elapsed);

    Ok((app, phase))
}

/// Phase 2: type into a real file and draw every frame.
fn editing(
    app: &mut App,
    fixture: &Fixture,
    options: &Options,
    budgets: Budgets,
) -> Result<PhaseReport> {
    let started = Instant::now();
    let mut phase = PhaseReport::new("editing");

    let target = fixture.root.join("src/stats.rs");
    app.open(&target).context("could not open src/stats.rs")?;
    app.act(Action::Move(Motion::DocumentEnd), false);

    let mut latencies = Vec::with_capacity(options.keystrokes);
    let text = "\n/// Added by the stress harness.\npub fn stressed(value: f64) -> f64 {\n    value * 2.0\n}\n";

    let mut typed = 0usize;
    while typed < options.keystrokes {
        for c in text.chars() {
            if typed >= options.keystrokes {
                break;
            }
            let event = if c == '\n' { KeyEvent::new(Key::Enter) } else { KeyEvent::char(c) };

            // The measurement that matters: from delivering the key to having
            // finished pixels. Anything less than the whole path is a number
            // that looks good and means nothing.
            let at = Instant::now();
            app.key(&event);
            app.frame(1.0)?;
            latencies.push(at.elapsed());

            typed += 1;
        }
    }

    // Multi-cursor, undo and search are the operations most likely to be
    // accidentally quadratic, so the phase exercises them too.
    let cursors = Instant::now();
    app.act(Action::Move(Motion::DocumentStart), false);
    for _ in 0..64 {
        app.act(Action::AddCursorBelow, false);
    }
    app.act(Action::Insert("// ".to_string()), false);
    app.frame(1.0)?;
    let multi_cursor = cursors.elapsed();

    let undo_start = Instant::now();
    let mut undone = 0;
    while matches!(app.act(Action::Undo, false), nebula_ide::Response::Redraw) {
        undone += 1;
        if undone > 5_000 {
            break;
        }
    }
    let undo = undo_start.elapsed();

    let elapsed = started.elapsed();
    let budget = budgets.frame(app.is_hardware_accelerated());

    phase.note(format!("{typed} keystrokes, each followed by a full frame"));
    phase.measure("keystroke-to-photon-p50", percentile(&latencies, 0.50), Some(budget));
    phase.measure("keystroke-to-photon-p95", percentile(&latencies, 0.95), Some(budget));
    phase.measure("keystroke-to-photon-p99", percentile(&latencies, 0.99), None);
    phase.measure("keystroke-to-photon-worst", percentile(&latencies, 1.0), None);
    phase.measure("64-cursor-edit", multi_cursor, None);
    phase.measure(format!("undo-{undone}-steps"), undo, None);
    phase.finish(elapsed);

    Ok(phase)
}

/// Phase 3: the same work, on a file large enough to break a naive editor.
fn large_file(
    app: &mut App,
    fixture: &Fixture,
    options: &Options,
    budgets: Budgets,
) -> Result<PhaseReport> {
    let started = Instant::now();
    let mut phase = PhaseReport::new("large-file");

    let open_start = Instant::now();
    app.open(fixture.generated()).context("could not open the generated file")?;
    let open = open_start.elapsed();

    let lines = app.workspace.document().buffer().len_lines();
    let chars = app.workspace.document().buffer().len_chars();
    phase.note(format!("{lines} lines, {chars} characters"));

    let first_frame = Instant::now();
    app.frame(1.0)?;
    let first = first_frame.elapsed();

    // Scroll the whole way through. A viewport that is not really a viewport
    // shows up here as a slope rather than a flat line.
    let mut scroll_frames = Vec::new();
    let page = app.view.viewport.visible_lines.max(1) as isize;
    let pages = (lines as isize / page).clamp(1, 400);
    for _ in 0..pages {
        let at = Instant::now();
        app.act(Action::Scroll(page), false);
        app.frame(1.0)?;
        scroll_frames.push(at.elapsed());
    }

    // Type into the middle of it, which forces an incremental re-parse of a
    // 200 000-line tree.
    app.act(Action::Move(Motion::DocumentStart), false);
    for _ in 0..(lines / 2) {
        app.view.viewport.first_line = lines / 2;
    }
    let mut edit_frames = Vec::new();
    let sample = options.keystrokes.min(500);
    for c in std::iter::repeat_n('x', sample) {
        let at = Instant::now();
        app.key(&KeyEvent::char(c));
        app.frame(1.0)?;
        edit_frames.push(at.elapsed());
    }

    let elapsed = started.elapsed();
    let budget = budgets.frame(app.is_hardware_accelerated());

    phase.measure("open", open, None);
    phase.measure("first-frame", first, None);
    phase.measure("scroll-frame-p95", percentile(&scroll_frames, 0.95), Some(budget));
    phase.measure("scroll-frame-worst", percentile(&scroll_frames, 1.0), None);
    phase.measure("keystroke-to-photon-p95", percentile(&edit_frames, 0.95), Some(budget));
    phase.measure("keystroke-to-photon-worst", percentile(&edit_frames, 1.0), None);
    phase.finish(elapsed);

    Ok(phase)
}

/// Phase 4: build every index the editor and the agent rely on.
fn indexing(fixture: &Fixture) -> Result<PhaseReport> {
    let started = Instant::now();
    let mut phase = PhaseReport::new("indexing");

    let project = nebula_vfs::Project::open(&fixture.root)?;

    let walk_start = Instant::now();
    let files = project.files()?;
    phase.measure("walk", walk_start.elapsed(), None);
    phase.note(format!("{} files", files.len()));

    let map_start = Instant::now();
    let options = nebula_index::RepoMapOptions::default();
    let map = nebula_index::RepoMap::build(&project, &options)?;
    let rendered = map.render(&options);
    phase.measure("repo-map", map_start.elapsed(), None);
    phase.note(format!("{} ranked files, {} bytes rendered", map.len(), rendered.len()));

    anyhow::ensure!(!map.is_empty(), "the repo map ranked nothing at all");

    let search_start = Instant::now();
    let searcher = nebula_search::ContentSearcher::new();
    let query = nebula_search::SearchQuery::literal("pub fn").max_results(10_000);
    let results = searcher.search(&project, &query)?;
    phase.measure("content-search", search_start.elapsed(), None);
    phase.note(format!("{} matches for `pub fn`", results.matches.len()));

    anyhow::ensure!(!results.matches.is_empty(), "searching a tree full of Rust found no `pub fn`");

    // The vector index, over one embedding per source file.
    let embed_start = Instant::now();
    let embedder = nebula_vector::HashingEmbedder::default();
    let mut vectors = Vec::new();
    for (index, entry) in files.iter().filter(|e| !e.is_dir).enumerate() {
        let Ok(text) = std::fs::read_to_string(&entry.path) else { continue };
        // One embedding per file is enough to exercise the index; chunking is
        // the editor's job, not the harness's.
        vectors.push((index as u64, nebula_vector::Embedder::embed(&embedder, &text)?));
    }
    phase.measure("embed", embed_start.elapsed(), None);

    let hnsw_start = Instant::now();
    let dim = nebula_vector::Embedder::dim(&embedder);
    let mut index = nebula_vector::Hnsw::new(nebula_vector::HnswConfig::new(dim));
    index.insert_batch(vectors.clone())?;
    phase.measure("hnsw-build", hnsw_start.elapsed(), None);
    phase.note(format!("{} vectors indexed", index.len()));

    let query_start = Instant::now();
    let probe = nebula_vector::Embedder::embed(&embedder, "standard deviation of a sample")?;
    let neighbours = index.search(&probe, 5)?;
    phase.measure("hnsw-query", query_start.elapsed(), None);

    anyhow::ensure!(!neighbours.is_empty(), "the vector index returned nothing");

    phase.finish(started.elapsed());
    Ok(phase)
}

/// Phase 5: compile and run every program in the fixture.
fn programs(fixture: &Fixture, options: &Options) -> Result<(PhaseReport, Vec<ProgramRun>)> {
    let started = Instant::now();
    let mut phase = PhaseReport::new("programs");
    let mut runs = Vec::new();

    for program in &fixture.programs {
        match run_program(&fixture.root, program) {
            Ok(run) => {
                phase.measure(
                    format!("{}-total", program.language),
                    run.build.unwrap_or_default() + run.run,
                    None,
                );
                if !run.passed {
                    // The note carries how the process finished. Without it a
                    // program killed by the sandbox and one that exited
                    // quietly are the same empty string, which is exactly how
                    // the macOS run left five of six failures undiagnosable.
                    let how = run.note.as_deref().unwrap_or("no exit status recorded");
                    phase.fail(format!(
                        "{} printed {:?}, expected {:?} ({how})",
                        program.language, run.output, program.expect
                    ));
                }
                runs.push(run);
            }
            Err(ToolchainMissing) => {
                let message = format!("{} needs `{}`", program.language, program.requires);
                if options.require_all_toolchains {
                    phase.fail(format!("{message}, which is not on PATH"));
                } else {
                    phase.note(format!("skipped: {message}"));
                }
                runs.push(ProgramRun::skipped(program));
            }
        }
    }

    let ran = runs.iter().filter(|r| !r.skipped).count();
    phase.note(format!("{ran} of {} programs built and ran", fixture.programs.len()));
    phase.finish(started.elapsed());

    Ok((phase, runs))
}

/// The tool a program needs is not installed.
struct ToolchainMissing;

/// Build and run one program, under the sandbox.
fn run_program(
    root: &Path,
    program: &Program,
) -> std::result::Result<ProgramRun, ToolchainMissing> {
    if which(program.requires).is_none() {
        return Err(ToolchainMissing);
    }

    // The programs are the fixture's own, but they are still run confined: a
    // harness that grants its subjects more than the editor would is not
    // measuring the editor.
    let policy = nebula_sandbox::Policy::project_tool(root);

    let mut build_time = None;
    if let Some(argv) = &program.build {
        let started = Instant::now();
        let output = nebula_exec::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(root)
            .sandbox(policy.clone())
            .limits(nebula_exec::ResourceLimits::build())
            .inherit_env(true)
            .run_blocking();
        build_time = Some(started.elapsed());

        match output {
            Ok(output) if !output.is_success() => {
                return Ok(ProgramRun {
                    language: program.language.to_string(),
                    command: argv.join(" "),
                    build: build_time,
                    run: Duration::ZERO,
                    output: output.combined(),
                    passed: false,
                    skipped: false,
                    note: Some(format!("the build failed: {}", output.status)),
                });
            }
            Err(error) => {
                return Ok(ProgramRun {
                    language: program.language.to_string(),
                    command: argv.join(" "),
                    build: build_time,
                    run: Duration::ZERO,
                    output: error.to_string(),
                    passed: false,
                    skipped: false,
                    note: Some("the build could not be started".to_string()),
                });
            }
            _ => {}
        }
    }

    // A binary the build just produced sits in `root`, and naming it relatively
    // does not survive Windows: a bare name is looked up on `PATH`, and a
    // relative path is resolved against *this* process's working directory
    // rather than the child's, so `current_dir(root)` never applies to it. Unix
    // works by accident of `execve` resolving `./name` after the chdir. Made
    // absolute here, and only when the file is really there, so a program name
    // meant for `PATH` — `python3`, `node`, `sh` — is left alone.
    let mut argv = program.run.clone();
    if let Some(first) = argv.first_mut() {
        let local = root.join(first.trim_start_matches("./").trim_start_matches(".\\"));
        if local.is_file() {
            *first = local.display().to_string();
        }
    }

    let started = Instant::now();
    let output = nebula_exec::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(root)
        .sandbox(policy)
        .limits(nebula_exec::ResourceLimits::long_running())
        .inherit_env(true)
        .run_blocking();
    let run_time = started.elapsed();

    let (text, passed, note) = match output {
        Ok(output) => {
            let text = output.combined();
            let passed = output.is_success() && text.contains(program.expect);
            // On success the interesting fact is what confinement was in
            // force. On failure it is how the process died: a program the
            // sandbox kills writes nothing to either stream, so an empty
            // output plus "killed by signal 9" is the whole of the evidence,
            // and reporting only the empty output throws it away.
            let note = if passed {
                output.enforcement.clone()
            } else {
                Some(match &output.enforcement {
                    Some(enforcement) => format!("{}, under {enforcement}", output.status),
                    None => output.status.to_string(),
                })
            };
            (text, passed, note)
        }
        Err(error) => (error.to_string(), false, Some("could not be started".to_string())),
    };

    Ok(ProgramRun {
        language: program.language.to_string(),
        command: program.run.join(" "),
        build: build_time,
        run: run_time,
        output: text.trim().to_string(),
        passed,
        skipped: false,
        note,
    })
}

/// Phase 6: everything edited is on disk and reads back unchanged.
fn durability(app: &mut App, fixture: &Fixture) -> Result<PhaseReport> {
    let started = Instant::now();
    let mut phase = PhaseReport::new("durability");

    let mut saved = 0;
    for index in 0..app.workspace.len() {
        app.workspace.focus(index);
        if app.workspace.document().path().is_none() {
            continue;
        }

        let expected = app.workspace.document().to_bytes();
        let path = match app.workspace.save() {
            Ok(path) => path,
            Err(nebula_ide::IdeError::NoPath) => continue,
            Err(error) => return Err(error.into()),
        };

        let written = std::fs::read(&path)
            .with_context(|| format!("could not read back {}", path.display()))?;
        anyhow::ensure!(
            written == expected,
            "{} does not match what the editor holds",
            path.display()
        );
        saved += 1;
    }

    // The fixture's own files must still parse after everything the harness
    // typed into them, or the run proved nothing about correctness.
    let registry = nebula_syntax::GrammarRegistry::new();
    let mut checked = 0;
    for path in fixture.sources() {
        let Some(language) = nebula_core::document::detect_language(&path) else { continue };
        let Ok(grammar) = registry.get(&language) else { continue };

        let bytes = std::fs::read(&path)?;
        let buffer = nebula_core::TextBuffer::from_bytes(&bytes)?;
        let tree = nebula_syntax::SyntaxTree::parse(grammar, &buffer, 0)?;
        if tree.has_error() {
            phase.fail(format!("{} no longer parses as {language}", path.display()));
        }
        checked += 1;
    }

    phase.note(format!("{saved} files saved and verified, {checked} re-parsed"));
    phase.finish(started.elapsed());
    Ok(phase)
}

/// The value `fraction` of the samples come in under.
fn percentile(samples: &[Duration], fraction: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index]
}

/// Find an executable on `PATH`.
///
/// Hand-rolled rather than a dependency: it is the whole of `which` that the
/// harness needs, and a missing toolchain is a routine outcome here rather than
/// an error worth a crate.
fn which(program: &str) -> Option<std::path::PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) {
        let path = std::path::PathBuf::from(program);
        return path.is_file().then_some(path);
    }

    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(program);
        if is_executable(&candidate) {
            return Some(candidate);
        }

        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat"] {
            let candidate = directory.join(format!("{program}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// The tools present on this machine, for the report's header.
pub fn available_toolchains() -> Vec<(String, bool)> {
    ["rustc", "cc", "go", "python3", "node", "sh"]
        .iter()
        .map(|tool| (tool.to_string(), which(tool).is_some()))
        .collect()
}

/// Machine facts worth recording alongside the numbers.
pub fn environment() -> Timings {
    Timings {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpus: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        sandbox: nebula_sandbox::backend_description(),
        sandbox_enforced: nebula_sandbox::is_available(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn small(dir: &TempDir) -> Options {
        Options {
            workdir: dir.path().join("fixture"),
            generated_lines: 2_000,
            keystrokes: 40,
            surface: (400, 300),
            force_cpu: true,
            skip_programs: true,
            require_all_toolchains: false,
            enforce_budgets: true,
        }
    }

    #[test]
    fn a_whole_run_completes_and_reports_every_phase() {
        let dir = TempDir::new().unwrap();
        let report = run(&small(&dir)).unwrap();

        let names: Vec<&str> = report.phases.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["cold-start", "editing", "large-file", "indexing", "programs", "durability"]
        );
        assert!(report.elapsed > Duration::ZERO);
    }

    #[test]
    fn a_clean_run_reports_no_correctness_failures() {
        // Correctness only. The timing budgets are deliberately *not* asserted
        // here: this test runs alongside thirty-odd others, each driving its own
        // editor on the same machine, so a frame time measured under that load
        // says nothing about the editor. Budgets are the job of a real run,
        // where the harness has the machine to itself — `nebula-stress` exits
        // non-zero on an overrun, and CI runs it.
        let dir = TempDir::new().unwrap();
        let report = run(&small(&dir)).unwrap();

        let failures: Vec<&String> =
            report.phases.iter().flat_map(|phase| &phase.failures).collect();
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn every_phase_is_held_to_a_budget_somewhere() {
        // The counterpart to the test above: budgets are not asserted there, so
        // this checks they exist at all. A phase that measures nothing against
        // a budget can never fail a real run.
        let dir = TempDir::new().unwrap();
        let report = run(&small(&dir)).unwrap();

        for name in ["cold-start", "editing", "large-file"] {
            let phase = report.phase(name).unwrap();
            assert!(
                phase.measurements.iter().any(|m| m.budget.is_some()),
                "{name} has no measurement with a budget"
            );
        }
    }

    #[test]
    fn the_editing_phase_measures_the_whole_keystroke_path() {
        let dir = TempDir::new().unwrap();
        let report = run(&small(&dir)).unwrap();

        let editing = report.phase("editing").unwrap();
        for name in ["keystroke-to-photon-p50", "keystroke-to-photon-p95"] {
            let measurement = editing
                .measurements
                .iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("{name} was not measured"));
            assert!(measurement.elapsed > Duration::ZERO);
            assert!(measurement.budget.is_some(), "{name} has no budget to fail");
        }
    }

    #[test]
    fn the_large_file_phase_actually_opens_a_large_file() {
        let dir = TempDir::new().unwrap();
        let report = run(&small(&dir)).unwrap();

        let phase = report.phase("large-file").unwrap();
        let note = phase.notes.join(" ");
        let lines: usize = note
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .expect("the phase should report a line count");
        assert!(lines > 2_000, "only {lines} lines");
    }

    #[test]
    fn a_large_file_frame_is_not_slower_than_a_small_one_by_an_order_of_magnitude() {
        // This is the design claim the viewport exists to make true. A generous
        // factor, because the harness shares a machine with whatever else CI is
        // running; a regression that breaks the property breaks it by 100x, not
        // by 20%.
        let dir = TempDir::new().unwrap();
        let report = run(&small(&dir)).unwrap();

        let small_p95 = report
            .phase("editing")
            .unwrap()
            .measurement("keystroke-to-photon-p95")
            .unwrap()
            .elapsed;
        let large_p95 = report
            .phase("large-file")
            .unwrap()
            .measurement("keystroke-to-photon-p95")
            .unwrap()
            .elapsed;

        assert!(
            large_p95 <= small_p95 * 10 + Duration::from_millis(5),
            "a keystroke costs {large_p95:?} on the large file against {small_p95:?} on the small one"
        );
    }

    #[test]
    fn the_indexing_phase_finds_real_symbols() {
        let dir = TempDir::new().unwrap();
        let report = run(&small(&dir)).unwrap();

        let notes = report.phase("indexing").unwrap().notes.join(" ");
        assert!(notes.contains("ranked files"), "{notes}");
        assert!(notes.contains("matches for `pub fn`"), "{notes}");
        assert!(notes.contains("vectors indexed"), "{notes}");
    }

    #[test]
    fn the_durability_phase_verifies_what_it_wrote() {
        let dir = TempDir::new().unwrap();
        let report = run(&small(&dir)).unwrap();

        let notes = report.phase("durability").unwrap().notes.join(" ");
        assert!(notes.contains("saved and verified"), "{notes}");
    }

    #[test]
    fn edits_survive_the_run_and_are_on_disk() {
        let dir = TempDir::new().unwrap();
        let options = small(&dir);
        run(&options).unwrap();

        let text = std::fs::read_to_string(options.workdir.join("src/stats.rs")).unwrap();
        // The editing phase types, then undoes everything it typed, so what has
        // to survive is the original content — intact, not truncated.
        assert!(text.contains("pub struct Summary"), "the file was damaged");
        assert!(text.contains("pub fn of(samples: &[f64])"), "the file was damaged");
    }

    #[test]
    fn skipping_the_program_phase_is_recorded_rather_than_silent() {
        let dir = TempDir::new().unwrap();
        let report = run(&small(&dir)).unwrap();

        let phase = report.phase("programs").unwrap();
        assert!(phase.skipped, "a skipped phase must say so");
        assert!(report.programs.is_empty());
    }

    #[test]
    fn programs_run_for_real_when_the_phase_is_enabled() {
        // At minimum `sh` exists everywhere this can run, so the phase always
        // has something to execute.
        let dir = TempDir::new().unwrap();
        let mut options = small(&dir);
        options.skip_programs = false;

        let report = run(&options).unwrap();
        let shell = report
            .programs
            .iter()
            .find(|run| run.language == "bash")
            .expect("the shell program was not attempted");

        assert!(!shell.skipped, "sh should be available");
        assert!(shell.passed, "sh printed {:?}", shell.output);
        assert!(shell.output.contains("sources=12"), "{}", shell.output);
    }

    #[test]
    fn a_missing_toolchain_is_skipped_not_failed() {
        let program = Program {
            language: "imaginary",
            entry: "nowhere".into(),
            build: None,
            run: vec!["definitely-not-a-real-program-xyz".into()],
            expect: "nothing",
            requires: "definitely-not-a-real-program-xyz",
        };
        let dir = TempDir::new().unwrap();
        assert!(run_program(dir.path(), &program).is_err());
    }

    #[test]
    fn which_finds_a_real_program_and_not_an_imaginary_one() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-program-xyz").is_none());
    }

    #[test]
    fn percentiles_are_ordered() {
        let samples: Vec<Duration> = (1..=100).map(Duration::from_micros).collect();
        assert!(percentile(&samples, 0.5) < percentile(&samples, 0.95));
        assert_eq!(percentile(&samples, 1.0), Duration::from_micros(100));
        assert_eq!(percentile(&[], 0.5), Duration::ZERO);
    }

    #[test]
    fn the_budgets_come_from_the_renderer_not_from_here() {
        let budgets = Budgets::default();
        assert_eq!(budgets.frame(true), nebula_render::budget::KEYSTROKE_TO_PHOTON_GPU);
        assert_eq!(budgets.frame(false), nebula_render::budget::KEYSTROKE_TO_PHOTON_CPU);
    }

    #[test]
    fn the_environment_is_recorded() {
        let environment = environment();
        assert!(!environment.os.is_empty());
        assert!(!environment.arch.is_empty());
        assert!(environment.cpus >= 1);
        assert!(!environment.sandbox.is_empty());
    }

    #[test]
    fn every_toolchain_is_probed() {
        let tools = available_toolchains();
        assert_eq!(tools.len(), 6);
        assert!(tools.iter().any(|(name, present)| name == "sh" && *present));
    }

    #[test]
    fn unused_modifiers_do_not_change_typed_text() {
        // Guards the harness itself: if `KeyEvent::char` ever started carrying
        // a modifier, every keystroke in the editing phase would silently
        // become a command and the phase would measure nothing.
        assert_eq!(KeyEvent::char('a').modifiers, nebula_ui::Modifiers::NONE);
    }
}
