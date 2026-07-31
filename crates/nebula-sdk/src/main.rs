//! The `nebula-sdk` command-line tool.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use console::style;
use nebula_pkg::{KeyPair, NotarizationVerdict};
use nebula_sdk::{Project, Result, SdkError, scaffold};

/// Build and publish Nebula IDE extensions.
#[derive(Debug, Parser)]
#[command(name = "nebula-sdk", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new extension project.
    New {
        /// Where to create it.
        directory: PathBuf,
        /// The reverse-DNS identifier, e.g. `com.example.formatter`.
        #[arg(long)]
        id: String,
        /// The publisher's name.
        #[arg(long, default_value = "Unknown Author")]
        author: String,
    },

    /// Compile the extension to a WebAssembly component.
    Build {
        /// Build with optimisations.
        #[arg(long)]
        release: bool,
    },

    /// Load the extension into a host identical to the editor's and activate it.
    Test {
        /// Build with optimisations first.
        #[arg(long)]
        release: bool,
    },

    /// Run the registry's notarisation locally.
    Check {
        /// Build with optimisations first.
        #[arg(long)]
        release: bool,
    },

    /// Build, check, sign and write a package.
    Package {
        /// Build with optimisations.
        #[arg(long, default_value_t = true)]
        release: bool,
        /// The publisher key, base64-encoded.
        #[arg(long, env = "NEBULA_PUBLISHER_KEY")]
        key: PathBuf,
    },

    /// Publish a package to a registry.
    Publish {
        /// The publisher key, base64-encoded.
        #[arg(long, env = "NEBULA_PUBLISHER_KEY")]
        key: PathBuf,
        /// The registry to publish to.
        #[arg(long, default_value = "https://extensions.nebula.dev", env = "NEBULA_REGISTRY")]
        registry: String,
    },

    /// Generate a publisher key pair.
    Keygen {
        /// Where to write the private key.
        #[arg(long)]
        out: PathBuf,
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
        // The message is the product here: a developer hitting a build failure
        // should not have to read a backtrace to find out what to fix.
        eprintln!("{} {error}", style("error:").red().bold());
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::New { directory, id, author } => {
            let path = scaffold::create(&directory, &id, &author)?;
            println!("{} {}", style("Created").green().bold(), path.display());
            println!();
            println!("  cd {}", path.display());
            println!("  rustup target add {}", nebula_sdk::WASM_TARGET);
            println!("  nebula-sdk build");
            Ok(())
        }

        Command::Build { release } => {
            let project = Project::discover(std::env::current_dir()?)?;
            let built = project.build(release)?;
            println!(
                "{} {} ({} bytes)",
                style("Built").green().bold(),
                built.component_path.display(),
                built.component.len()
            );
            Ok(())
        }

        Command::Test { release } => {
            let project = Project::discover(std::env::current_dir()?)?;
            let built = project.build(release)?;
            run_in_host(&project, &built.component)
        }

        Command::Check { release } => {
            let project = Project::discover(std::env::current_dir()?)?;
            let built = project.build(release)?;
            let report = project.check(&built.component)?;
            print_report(&report);
            Ok(())
        }

        Command::Package { release, key } => {
            let project = Project::discover(std::env::current_dir()?)?;
            let keys = load_key(&key)?;
            let path = project.package(release, &keys)?;
            println!("{} {}", style("Packaged").green().bold(), path.display());
            println!("  signed by {}", keys.public().fingerprint());
            Ok(())
        }

        Command::Publish { key, registry } => {
            let project = Project::discover(std::env::current_dir()?)?;
            let keys = load_key(&key)?;
            let path = project.package(true, &keys)?;
            publish(&path, &registry)
        }

        Command::Keygen { out } => {
            let keys = KeyPair::generate();
            if out.exists() {
                return Err(SdkError::AlreadyExists(out));
            }
            std::fs::write(&out, keys.to_private_base64())?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // A signing key readable by every user on the machine is a
                // signing key that is not really yours.
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o600))?;
            }

            println!("{} {}", style("Wrote").green().bold(), out.display());
            println!();
            println!("  public key:  {}", keys.public().to_base64());
            println!("  fingerprint: {}", keys.public().fingerprint());
            println!();
            println!("Send the public key to the registry operator so publishing is accepted.");
            println!("{} the private key is not recoverable. Back it up.", style("Note:").yellow());
            Ok(())
        }
    }
}

/// Load a private key from disk.
fn load_key(path: &PathBuf) -> Result<KeyPair> {
    let contents = std::fs::read_to_string(path)?;
    KeyPair::from_private_base64(&contents)
        .map_err(|e| SdkError::Package(nebula_pkg::PkgError::Signature(e)))
}

/// Load the extension into the real host and activate it.
fn run_in_host(project: &Project, component: &[u8]) -> Result<()> {
    let host = nebula_wasm_host::ExtensionHost::new()
        .map_err(|e| SdkError::WouldBeRejected(e.to_string()))?;

    let granted: std::collections::BTreeSet<_> =
        project.manifest.capabilities.iter().copied().collect();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SdkError::Io)?;

    runtime.block_on(async {
        let mut instance = host
            .start(
                &project.manifest.id,
                component,
                &granted,
                nebula_wasm_host::ExecutionLimits::background(),
            )
            .await
            .map_err(|e| SdkError::WouldBeRejected(e.to_string()))?;

        println!("{} the extension loaded", style("✓").green());

        match instance.call("activate", &[]).await {
            Ok(_) => {
                println!("{} activate() returned successfully", style("✓").green());
                Ok(())
            }
            Err(error) => Err(SdkError::WouldBeRejected(format!("activate() failed: {error}"))),
        }
    })
}

/// Print a notarisation report the way a developer needs to read it.
fn print_report(report: &nebula_pkg::NotarizationReport) {
    println!("{} {}", style("Checked").green().bold(), report.extension_id);
    println!("  component: {} bytes", report.component_bytes);

    if !report.required.is_empty() {
        let names: Vec<&str> = report.required.iter().map(|c| c.name()).collect();
        println!("  imports:   {}", names.join(", "));
    }
    if !report.over_declared.is_empty() {
        let names: Vec<&str> = report.over_declared.iter().map(|c| c.name()).collect();
        println!(
            "  {} declared but never imported: {}",
            style("warning:").yellow(),
            names.join(", ")
        );
        println!("            this makes the install prompt scarier than it needs to be");
    }

    match &report.verdict {
        NotarizationVerdict::Approved => {
            println!("{} ready to publish", style("✓").green().bold());
        }
        NotarizationVerdict::NeedsReview { reasons } => {
            println!("{} this will be held for human review because it:", style("!").yellow().bold());
            for reason in reasons {
                println!("    • {reason}");
            }
        }
        NotarizationVerdict::Rejected { reasons } => {
            println!("{} this would be rejected:", style("✗").red().bold());
            for reason in reasons {
                println!("    • {reason}");
            }
        }
    }
}

/// Upload a package to a registry.
fn publish(path: &PathBuf, registry: &str) -> Result<()> {
    let bytes = std::fs::read(path)?;
    let url = format!("{}/v1/publish", registry.trim_end_matches('/'));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SdkError::Io)?;

    runtime.block_on(async {
        let response = reqwest::Client::new()
            .post(&url)
            .body(bytes)
            .send()
            .await
            .map_err(|e| SdkError::Publish(format!("could not reach {registry}: {e}")))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            // The registry's error body names the specific problem; passing it
            // through unchanged is more useful than re-wording it.
            return Err(SdkError::Publish(format!("the registry returned {status}: {body}")));
        }

        let outcome: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        match outcome.get("outcome").and_then(|v| v.as_str()) {
            Some("held-for-review") => {
                println!("{} submitted, and held for review because it:", style("!").yellow().bold());
                if let Some(reasons) = outcome.get("reasons").and_then(|v| v.as_array()) {
                    for reason in reasons {
                        println!("    • {}", reason.as_str().unwrap_or_default());
                    }
                }
                println!("  You will be notified when a reviewer has looked at it.");
            }
            _ => println!("{} published", style("✓").green().bold()),
        }
        Ok(())
    })
}
