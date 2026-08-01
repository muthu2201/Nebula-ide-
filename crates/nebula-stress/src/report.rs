//! What the run produced.
//!
//! The report is the deliverable: a run that passes but cannot say what it
//! measured is not evidence of anything. It serialises to JSON for CI to gate
//! on, and renders to Markdown for a human to read.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::fixture::Fixture;
use crate::harness::Budgets;

/// One measured number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurement {
    /// What was measured.
    pub name: String,
    /// How long it took.
    #[serde(with = "millis")]
    pub elapsed: Duration,
    /// The budget it is held to, if any.
    #[serde(with = "millis_option", skip_serializing_if = "Option::is_none", default)]
    pub budget: Option<Duration>,
}

impl Measurement {
    /// Whether this measurement is inside its budget. A measurement with no
    /// budget is informational and never fails a run.
    pub fn within_budget(&self) -> bool {
        self.budget.is_none_or(|budget| self.elapsed <= budget)
    }
}

/// One phase of the run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseReport {
    /// The phase's name.
    pub name: String,
    /// What it measured.
    pub measurements: Vec<Measurement>,
    /// Anything worth saying in prose.
    pub notes: Vec<String>,
    /// Anything that went wrong.
    pub failures: Vec<String>,
    /// How long the whole phase took.
    #[serde(with = "millis")]
    pub elapsed: Duration,
    /// Whether the phase was skipped.
    pub skipped: bool,
}

impl PhaseReport {
    /// A phase about to run.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            measurements: Vec::new(),
            notes: Vec::new(),
            failures: Vec::new(),
            elapsed: Duration::ZERO,
            skipped: false,
        }
    }

    /// A phase that did not run, and why.
    pub fn skipped(name: impl Into<String>, reason: impl Into<String>) -> Self {
        let mut phase = Self::new(name);
        phase.skipped = true;
        phase.notes.push(reason.into());
        phase
    }

    /// Record a number.
    pub fn measure(
        &mut self,
        name: impl Into<String>,
        elapsed: Duration,
        budget: Option<Duration>,
    ) {
        self.measurements.push(Measurement { name: name.into(), elapsed, budget });
    }

    /// Record something worth saying.
    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    /// Record something that went wrong.
    pub fn fail(&mut self, failure: impl Into<String>) {
        self.failures.push(failure.into());
    }

    /// Close the phase.
    pub fn finish(&mut self, elapsed: Duration) {
        self.elapsed = elapsed;
    }

    /// One measurement by name.
    pub fn measurement(&self, name: &str) -> Option<&Measurement> {
        self.measurements.iter().find(|m| m.name == name)
    }

    /// Whether everything in this phase held.
    pub fn passed(&self) -> bool {
        self.failures.is_empty() && self.measurements.iter().all(Measurement::within_budget)
    }

    /// Every reason this phase did not pass, in words.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = self.failures.clone();
        for measurement in &self.measurements {
            if let Some(budget) = measurement.budget
                && measurement.elapsed > budget
            {
                problems.push(format!(
                    "{}/{} took {:.2?}, over its {:.2?} budget",
                    self.name, measurement.name, measurement.elapsed, budget
                ));
            }
        }
        problems
    }
}

/// One program that was built and run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramRun {
    /// Which language.
    pub language: String,
    /// The command that ran.
    pub command: String,
    /// How long the build took, if there was one.
    #[serde(with = "millis_option", skip_serializing_if = "Option::is_none", default)]
    pub build: Option<Duration>,
    /// How long the program took.
    #[serde(with = "millis")]
    pub run: Duration,
    /// What it printed.
    pub output: String,
    /// Whether the output was what the fixture expects.
    pub passed: bool,
    /// Whether it was skipped because its toolchain is absent.
    pub skipped: bool,
    /// Anything else worth recording, including the sandbox's verdict.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
}

impl ProgramRun {
    /// A program that could not be attempted.
    pub fn skipped(program: &crate::fixture::Program) -> ProgramRun {
        ProgramRun {
            language: program.language.to_string(),
            command: program.run.join(" "),
            build: None,
            run: Duration::ZERO,
            output: String::new(),
            passed: false,
            skipped: true,
            note: Some(format!("`{}` is not on PATH", program.requires)),
        }
    }
}

/// Facts about the machine the run happened on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timings {
    /// Operating system.
    pub os: String,
    /// CPU architecture.
    pub arch: String,
    /// How many hardware threads.
    pub cpus: usize,
    /// Which sandbox backend is present.
    pub sandbox: String,
    /// Whether it can actually enforce anything here.
    pub sandbox_enforced: bool,
}

/// The whole run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    /// When it ran, as an RFC 3339 timestamp.
    pub timestamp: String,
    /// The version of the editor under test.
    pub version: String,
    /// Which renderer was used.
    pub renderer: String,
    /// Why that renderer was chosen.
    pub renderer_reason: String,
    /// The machine.
    pub environment: Timings,
    /// The budgets the run was held to.
    pub budgets: BudgetReport,
    /// How big the fixture was.
    pub fixture: FixtureReport,
    /// Each phase.
    pub phases: Vec<PhaseReport>,
    /// Each program.
    pub programs: Vec<ProgramRun>,
    /// How long the whole run took.
    #[serde(with = "millis")]
    pub elapsed: Duration,
    /// Whether everything held.
    pub passed: bool,
}

/// The budgets, in a serialisable shape.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BudgetReport {
    /// Keystroke to photon on the GPU path, in milliseconds.
    pub keystroke_to_photon_gpu_ms: f64,
    /// Keystroke to photon on the CPU path, in milliseconds.
    pub keystroke_to_photon_cpu_ms: f64,
    /// Cold start, in milliseconds.
    pub cold_start_ms: f64,
}

/// What the fixture contained.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureReport {
    /// Where it was written.
    pub root: String,
    /// How many files.
    pub files: usize,
    /// How many lines.
    pub lines: usize,
    /// How many programs it offers.
    pub programs: usize,
}

impl Report {
    /// An empty report for a run about to start.
    pub fn new(fixture: &Fixture, budgets: Budgets) -> Report {
        Report {
            timestamp: timestamp(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            renderer: "unknown".to_string(),
            renderer_reason: String::new(),
            environment: crate::harness::environment(),
            budgets: BudgetReport {
                keystroke_to_photon_gpu_ms: millis_of(budgets.keystroke_to_photon_gpu),
                keystroke_to_photon_cpu_ms: millis_of(budgets.keystroke_to_photon_cpu),
                cold_start_ms: millis_of(budgets.cold_start),
            },
            fixture: FixtureReport {
                root: fixture.root.display().to_string(),
                files: fixture.files,
                lines: fixture.lines,
                programs: fixture.programs.len(),
            },
            phases: Vec::new(),
            programs: Vec::new(),
            elapsed: Duration::ZERO,
            passed: false,
        }
    }

    /// Compute the overall verdict.
    pub fn finish(&mut self) {
        self.passed = self.failures().is_empty();
    }

    /// One phase by name.
    pub fn phase(&self, name: &str) -> Option<&PhaseReport> {
        self.phases.iter().find(|p| p.name == name)
    }

    /// Whether the run passed.
    pub fn passed(&self) -> bool {
        self.failures().is_empty()
    }

    /// Everything that went wrong, in words.
    pub fn failures(&self) -> Vec<String> {
        let mut failures: Vec<String> =
            self.phases.iter().filter(|p| !p.skipped).flat_map(PhaseReport::problems).collect();

        for program in &self.programs {
            if !program.skipped && !program.passed {
                failures.push(format!(
                    "the {} program did not produce the expected output",
                    program.language
                ));
            }
        }

        failures
    }

    /// Render as Markdown, for a human or for a job summary.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();

        out.push_str("# Nebula end-to-end stress run\n\n");
        out.push_str(&format!(
            "**{}** — {} on {} {}, {} hardware threads, {} renderer.\n\n",
            if self.passed { "PASSED" } else { "FAILED" },
            self.timestamp,
            self.environment.os,
            self.environment.arch,
            self.environment.cpus,
            self.renderer
        ));
        out.push_str(&format!("Renderer selection: {}\n\n", self.renderer_reason));
        out.push_str(&format!(
            "Sandbox: {} ({})\n\n",
            self.environment.sandbox,
            if self.environment.sandbox_enforced {
                "enforcing"
            } else {
                "not available on this kernel; programs ran unconfined"
            }
        ));
        out.push_str(&format!(
            "Fixture: {} files, {} lines, {} runnable programs.\n\n",
            self.fixture.files, self.fixture.lines, self.fixture.programs
        ));
        out.push_str(&format!("Total time: {:.2?}\n\n", self.elapsed));

        if !self.passed {
            out.push_str("## Failures\n\n");
            for failure in self.failures() {
                out.push_str(&format!("- {failure}\n"));
            }
            out.push('\n');
        }

        out.push_str("## Phases\n\n");
        for phase in &self.phases {
            let verdict = if phase.skipped {
                "skipped"
            } else if phase.passed() {
                "ok"
            } else {
                "FAILED"
            };
            out.push_str(&format!(
                "### {} — {verdict} ({:.2?})\n\n",
                phase.name, phase.elapsed
            ));

            for note in &phase.notes {
                out.push_str(&format!("{note}\n\n"));
            }

            if !phase.measurements.is_empty() {
                out.push_str("| Measurement | Time | Budget | |\n");
                out.push_str("| --- | ---: | ---: | :--- |\n");
                for measurement in &phase.measurements {
                    let budget = match measurement.budget {
                        Some(budget) => format!("{budget:.2?}"),
                        None => "—".to_string(),
                    };
                    let verdict = match measurement.budget {
                        None => "",
                        Some(_) if measurement.within_budget() => "within",
                        Some(_) => "**over**",
                    };
                    out.push_str(&format!(
                        "| {} | {:.2?} | {budget} | {verdict} |\n",
                        measurement.name, measurement.elapsed
                    ));
                }
                out.push('\n');
            }

            for failure in &phase.failures {
                out.push_str(&format!("- **{failure}**\n"));
            }
            if !phase.failures.is_empty() {
                out.push('\n');
            }
        }

        if !self.programs.is_empty() {
            out.push_str("## Programs\n\n");
            out.push_str("| Language | Command | Build | Run | Result | Output |\n");
            out.push_str("| --- | --- | ---: | ---: | --- | --- |\n");
            for program in &self.programs {
                let build = match program.build {
                    Some(build) => format!("{build:.2?}"),
                    None => "—".to_string(),
                };
                let result = if program.skipped {
                    "skipped"
                } else if program.passed {
                    "ok"
                } else {
                    "**FAILED**"
                };
                let output = program.output.replace('\n', " ⏎ ");
                let output = if output.len() > 80 {
                    format!("{}…", &output[..80])
                } else {
                    output
                };
                out.push_str(&format!(
                    "| {} | `{}` | {build} | {:.2?} | {result} | {output} |\n",
                    program.language, program.command, program.run
                ));
            }
            out.push('\n');
        }

        out
    }
}

fn millis_of(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// An RFC 3339 timestamp in UTC.
///
/// Computed from the epoch rather than pulled from a date crate: the report
/// needs one timestamp, and the civil-calendar arithmetic for that is short and
/// exact.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = now.as_secs();

    let (days, time) = (seconds / 86_400, seconds % 86_400);
    let (hour, minute, second) = (time / 3600, (time % 3600) / 60, time % 60);

    // Howard Hinnant's civil_from_days, shifted so the era starts in March.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Durations serialise as milliseconds, because that is the unit every
/// dashboard and every budget in this project is written in.
mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(value.as_secs_f64() * 1000.0)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let millis = f64::deserialize(d)?;
        Ok(Duration::from_secs_f64(millis.max(0.0) / 1000.0))
    }
}

mod millis_option {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => s.serialize_f64(value.as_secs_f64() * 1000.0),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let millis = Option::<f64>::deserialize(d)?;
        Ok(millis.map(|m| Duration::from_secs_f64(m.max(0.0) / 1000.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase_with_budget(elapsed: Duration, budget: Duration) -> PhaseReport {
        let mut phase = PhaseReport::new("example");
        phase.measure("thing", elapsed, Some(budget));
        phase.finish(elapsed);
        phase
    }

    #[test]
    fn a_measurement_inside_its_budget_passes() {
        let phase = phase_with_budget(Duration::from_millis(4), Duration::from_millis(8));
        assert!(phase.passed());
        assert!(phase.problems().is_empty());
    }

    #[test]
    fn a_measurement_over_its_budget_fails_and_says_by_how_much() {
        let phase = phase_with_budget(Duration::from_millis(40), Duration::from_millis(8));
        assert!(!phase.passed());

        let problems = phase.problems();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("over its"), "{}", problems[0]);
        assert!(problems[0].contains("example/thing"), "{}", problems[0]);
    }

    #[test]
    fn a_measurement_with_no_budget_never_fails_a_run() {
        let mut phase = PhaseReport::new("example");
        phase.measure("slow-but-informational", Duration::from_secs(60), None);
        assert!(phase.passed());
    }

    #[test]
    fn a_skipped_phase_does_not_fail_the_run() {
        let mut report = bare_report();
        report.phases.push(PhaseReport::skipped("programs", "no toolchains"));
        report.finish();
        assert!(report.passed);
    }

    #[test]
    fn a_failed_program_fails_the_run() {
        let mut report = bare_report();
        report.programs.push(ProgramRun {
            language: "rust".to_string(),
            command: "./thing".to_string(),
            build: Some(Duration::from_millis(200)),
            run: Duration::from_millis(3),
            output: "wrong".to_string(),
            passed: false,
            skipped: false,
            note: None,
        });
        report.finish();

        assert!(!report.passed);
        assert!(report.failures()[0].contains("rust"));
    }

    #[test]
    fn a_skipped_program_does_not_fail_the_run() {
        let mut report = bare_report();
        report.programs.push(ProgramRun {
            language: "go".to_string(),
            command: "go run .".to_string(),
            build: None,
            run: Duration::ZERO,
            output: String::new(),
            passed: false,
            skipped: true,
            note: Some("`go` is not on PATH".to_string()),
        });
        report.finish();
        assert!(report.passed);
    }

    #[test]
    fn a_report_survives_a_round_trip_through_json() {
        let mut report = bare_report();
        report.phases.push(phase_with_budget(
            Duration::from_micros(1_500),
            Duration::from_millis(8),
        ));
        report.finish();

        let json = serde_json::to_string_pretty(&report).unwrap();
        let restored: Report = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.passed, report.passed);
        assert_eq!(restored.phases.len(), 1);
        // Milliseconds as f64 round-trip exactly at this magnitude.
        assert_eq!(restored.phases[0].measurements[0].elapsed, Duration::from_micros(1_500));
    }

    #[test]
    fn the_markdown_names_the_verdict_and_the_numbers() {
        let mut report = bare_report();
        report.phases.push(phase_with_budget(
            Duration::from_millis(2),
            Duration::from_millis(8),
        ));
        report.programs.push(ProgramRun {
            language: "c".to_string(),
            command: "./sieve".to_string(),
            build: Some(Duration::from_millis(90)),
            run: Duration::from_millis(2),
            output: "primes below 1000: 168".to_string(),
            passed: true,
            skipped: false,
            note: None,
        });
        report.finish();

        let markdown = report.to_markdown();
        assert!(markdown.contains("PASSED"));
        assert!(markdown.contains("| thing |"));
        assert!(markdown.contains("primes below 1000: 168"));
        assert!(markdown.contains("`./sieve`"));
    }

    #[test]
    fn the_markdown_lists_failures_first() {
        let mut report = bare_report();
        report.phases.push(phase_with_budget(
            Duration::from_millis(80),
            Duration::from_millis(8),
        ));
        report.finish();

        let markdown = report.to_markdown();
        assert!(markdown.contains("FAILED"));
        let failures = markdown.find("## Failures").expect("no failures section");
        let phases = markdown.find("## Phases").expect("no phases section");
        assert!(failures < phases, "failures must come before the detail");
    }

    #[test]
    fn long_program_output_is_truncated_rather_than_wrapping_the_table() {
        let mut report = bare_report();
        report.programs.push(ProgramRun {
            language: "python".to_string(),
            command: "python3 x.py".to_string(),
            build: None,
            run: Duration::from_millis(10),
            output: "x".repeat(500),
            passed: true,
            skipped: false,
            note: None,
        });
        report.finish();

        for line in report.to_markdown().lines() {
            assert!(line.len() < 200, "a table row ran to {} characters", line.len());
        }
    }

    #[test]
    fn the_timestamp_is_a_plausible_rfc_3339_instant() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.ends_with('Z'), "{stamp}");

        let year: i32 = stamp[..4].parse().unwrap();
        assert!((2024..2200).contains(&year), "{stamp}");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");

        let month: u32 = stamp[5..7].parse().unwrap();
        let day: u32 = stamp[8..10].parse().unwrap();
        let hour: u32 = stamp[11..13].parse().unwrap();
        assert!((1..=12).contains(&month), "{stamp}");
        assert!((1..=31).contains(&day), "{stamp}");
        assert!(hour < 24, "{stamp}");
    }

    fn bare_report() -> Report {
        Report {
            timestamp: timestamp(),
            version: "0.1.0".to_string(),
            renderer: "cpu".to_string(),
            renderer_reason: "forced".to_string(),
            environment: crate::harness::environment(),
            budgets: BudgetReport {
                keystroke_to_photon_gpu_ms: 8.0,
                keystroke_to_photon_cpu_ms: 16.0,
                cold_start_ms: 500.0,
            },
            fixture: FixtureReport {
                root: "/tmp/fixture".to_string(),
                files: 17,
                lines: 1000,
                programs: 6,
            },
            phases: Vec::new(),
            programs: Vec::new(),
            elapsed: Duration::from_secs(1),
            passed: false,
        }
    }
}
