//! Project scaffolding.
//!
//! What `nebula-sdk new` writes has to compile, package and pass notarisation
//! as it stands. A template that needs three edits before it builds is where a
//! developer's first impression of the SDK is formed, and it is not a good one.

use std::path::{Path, PathBuf};

use crate::{Result, SdkError, WASM_TARGET};

/// Write a new extension project into `directory`.
pub fn create(directory: &Path, id: &str, author: &str) -> Result<PathBuf> {
    if directory.exists() {
        return Err(SdkError::AlreadyExists(directory.to_path_buf()));
    }

    let name = id.rsplit('.').next().unwrap_or("extension");
    let crate_name = name.replace(['-', '.'], "_");

    std::fs::create_dir_all(directory.join("src"))?;
    std::fs::create_dir_all(directory.join("wit"))?;
    std::fs::create_dir_all(directory.join("assets"))?;

    std::fs::write(directory.join(crate::project::MANIFEST_FILE), manifest(id, name, author))?;
    std::fs::write(directory.join("Cargo.toml"), cargo_toml(&crate_name))?;
    std::fs::write(directory.join("src/lib.rs"), lib_rs(name))?;
    std::fs::write(directory.join("wit/world.wit"), nebula_wasm_host::WitWorld::wit_source())?;
    std::fs::write(directory.join(".gitignore"), GITIGNORE)?;
    std::fs::write(directory.join("README.md"), readme(id, name))?;

    Ok(directory.to_path_buf())
}

fn manifest(id: &str, name: &str, author: &str) -> String {
    let title = title_case(name);
    format!(
        r#"# The extension's identity and what it may do.
#
# `capabilities` is checked against the component's real imports when the
# extension is notarised, so it has to match what the code actually uses —
# declaring more makes the install prompt scarier than it needs to be, and
# declaring less is a rejection.

id = "{id}"
name = "{title}"
version = "0.1.0"
description = "A Nebula extension"
license = "MIT"
world_version = "0.1.0"
capabilities = ["read-document", "register-commands"]
keywords = []

[author]
name = "{author}"

[[commands]]
id = "hello"
title = "{title}: Say Hello"
"#
    )
}

fn cargo_toml(crate_name: &str) -> String {
    format!(
        r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2021"
publish = false

# A component is a cdylib: the Component Model tooling turns it into one.
[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.46"

[profile.release]
# Extensions are downloaded, so size is worth more than the last few percent of
# speed. `opt-level = "s"` and LTO together typically halve the artefact.
opt-level = "s"
lto = true
strip = true
codegen-units = 1
"#
    )
}

fn lib_rs(name: &str) -> String {
    let title = title_case(name);
    format!(
        r#"//! A Nebula extension.
//!
//! Build with `nebula-sdk build`, which compiles to `{WASM_TARGET}` and packages
//! the result. Run `nebula-sdk check` to see exactly what the registry will
//! check before you publish.

wit_bindgen::generate!({{
    world: "extension",
    path: "wit",
}});

struct Extension;

impl Guest for Extension {{
    /// Called once when the extension loads.
    fn activate() -> Result<(), String> {{
        commands::register("hello", "{title}: Say Hello")
            .map_err(|error| format!("could not register the command: {{error:?}}"))?;
        Ok(())
    }}

    /// Called before the extension is unloaded.
    fn deactivate() {{}}

    /// Called when one of this extension's commands is invoked.
    fn handle_command(id: String, _arguments: String) -> Result<String, String> {{
        match id.as_str() {{
            "hello" => {{
                // `document::active` returns None when no file is focused, which
                // is a normal state rather than an error.
                match document::active() {{
                    Some(info) => Ok(format!(
                        "Hello from {title}. The open document is {{}} characters long.",
                        info.length
                    )),
                    None => Ok("Hello from {title}. No document is open.".to_string()),
                }}
            }}
            other => Err(format!("unknown command: {{other}}")),
        }}
    }}
}}

export!(Extension);
"#
    )
}

fn readme(id: &str, name: &str) -> String {
    let title = title_case(name);
    format!(
        r#"# {title}

A Nebula IDE extension.

## Building

```sh
rustup target add {WASM_TARGET}
nebula-sdk build
```

## Running it against a real host

```sh
nebula-sdk test
```

This loads the extension into the same Wasmtime host the editor uses, with the
same capability checks and the same execution limits, so behaviour here matches
behaviour in the editor.

## Checking before you publish

```sh
nebula-sdk check
```

Runs the registry's notarisation locally. It compares the capabilities declared
in `nebula.toml` against the ones the compiled component actually imports, which
is the check most likely to reject a first submission.

## Publishing

```sh
nebula-sdk publish --key ~/.nebula/publisher.key
```

The package is signed with your publisher key. The registry only accepts
signatures from keys registered to a publisher who owns the `{id}` namespace.
"#
    )
}

const GITIGNORE: &str = "/target\n*.nbx\n";

/// Capitalise each dash-separated word.
fn title_case(value: &str) -> String {
    value
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;
    use tempfile::TempDir;

    #[test]
    fn scaffolding_writes_every_file_a_project_needs() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("my-extension");
        create(&project, "com.example.formatter", "Example Author").unwrap();

        for expected in
            ["nebula.toml", "Cargo.toml", "src/lib.rs", "wit/world.wit", ".gitignore", "README.md"]
        {
            assert!(project.join(expected).is_file(), "{expected} was not written");
        }
        assert!(project.join("assets").is_dir());
    }

    #[test]
    fn the_scaffolded_manifest_is_valid() {
        // The template must not need editing before it works.
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("my-extension");
        create(&project, "com.example.formatter", "Example Author").unwrap();

        let discovered = Project::discover(&project).unwrap();
        assert_eq!(discovered.manifest.id, "com.example.formatter");
        assert_eq!(discovered.manifest.author.name, "Example Author");
        assert_eq!(discovered.manifest.name, "Formatter");
    }

    #[test]
    fn the_scaffolded_manifest_declares_the_capability_its_command_needs() {
        // Declaring a command without `register-commands` is a validation
        // failure, and shipping a template that trips it would be a poor start.
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("ext");
        create(&project, "com.example.thing", "Author").unwrap();

        let manifest = Project::discover(&project).unwrap().manifest;
        assert!(!manifest.commands.is_empty());
        assert!(manifest.capabilities.contains(&nebula_wasm_host::Capability::RegisterCommands));
    }

    #[test]
    fn the_scaffolded_world_matches_the_host() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("ext");
        create(&project, "com.example.thing", "Author").unwrap();

        let written = std::fs::read_to_string(project.join("wit/world.wit")).unwrap();
        assert_eq!(
            written,
            nebula_wasm_host::WitWorld::wit_source(),
            "a scaffolded project must target the host's actual world"
        );
    }

    #[test]
    fn the_scaffolded_crate_builds_a_cdylib() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("ext");
        create(&project, "com.example.thing", "Author").unwrap();

        let cargo = std::fs::read_to_string(project.join("Cargo.toml")).unwrap();
        assert!(cargo.contains(r#"crate-type = ["cdylib"]"#), "{cargo}");
        assert!(cargo.contains("wit-bindgen"));
    }

    #[test]
    fn the_scaffolded_source_implements_every_required_export() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("ext");
        create(&project, "com.example.thing", "Author").unwrap();

        let source = std::fs::read_to_string(project.join("src/lib.rs")).unwrap();
        for export in ["fn activate", "fn deactivate", "fn handle_command"] {
            assert!(source.contains(export), "the template must implement `{export}`");
        }
        assert!(source.contains("export!(Extension)"));
    }

    #[test]
    fn scaffolding_over_an_existing_directory_is_refused() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("existing");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("important.txt"), b"do not clobber me").unwrap();

        assert!(matches!(
            create(&project, "com.example.thing", "Author"),
            Err(SdkError::AlreadyExists(_))
        ));
        assert!(project.join("important.txt").exists());
    }

    #[test]
    fn identifiers_become_readable_titles() {
        assert_eq!(title_case("formatter"), "Formatter");
        assert_eq!(title_case("my-cool-extension"), "My Cool Extension");
        assert_eq!(title_case("snake_case_name"), "Snake Case Name");
        assert_eq!(title_case(""), "");
    }
}
