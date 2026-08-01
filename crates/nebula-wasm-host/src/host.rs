//! The Wasmtime host.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use wasmtime::component::{Component, Instance, Linker, Val};
use wasmtime::{Config, Engine, Store, Trap};

use crate::limits::{EPOCH_TICK, ExecutionLimits, ResourceCaps, StoreLimits};
use crate::world::{Capability, WitWorld, WorldVersion, capabilities_from_imports};
use crate::{Result, WasmError, anyhow_lite};

/// What a store carries alongside the guest.
pub struct HostState {
    limits: StoreLimits,
    /// The capabilities this extension was granted.
    granted: BTreeSet<Capability>,
    /// The extension's name, for error messages.
    name: String,
}

impl HostState {
    /// The capabilities granted to this extension.
    pub fn granted(&self) -> &BTreeSet<Capability> {
        &self.granted
    }

    /// Whether a capability is granted.
    ///
    /// Called by every host function before it does anything. The WIT world
    /// already stops an extension importing an interface it was not linked
    /// against, so this is the second of two checks — cheap, and it means a
    /// linking mistake is a refused call rather than an unguarded one.
    pub fn require(&self, capability: Capability) -> Result<()> {
        if self.granted.contains(&capability) {
            Ok(())
        } else {
            Err(WasmError::CapabilityNotGranted {
                name: self.name.clone(),
                capability: capability.name().to_string(),
            })
        }
    }

    /// Whether the guest hit its memory cap.
    pub fn hit_memory_limit(&self) -> bool {
        self.limits.hit_memory_limit()
    }
}

/// A compiled, not-yet-running extension.
pub struct LoadedExtension {
    name: String,
    component: Component,
    /// Capabilities derived from the component's real imports.
    required: BTreeSet<Capability>,
    /// The world version the component targets.
    world_version: WorldVersion,
}

impl LoadedExtension {
    /// The extension's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The capabilities the component's imports actually require.
    ///
    /// Derived from the binary, not from the manifest — which is exactly what
    /// makes it useful for notarisation.
    pub fn required_capabilities(&self) -> &BTreeSet<Capability> {
        &self.required
    }

    /// The world version it targets.
    pub fn world_version(&self) -> &WorldVersion {
        &self.world_version
    }
}

/// Compiles and runs extensions.
pub struct ExtensionHost {
    engine: Engine,
    world: WitWorld,
    caps: ResourceCaps,
    /// Stops the epoch ticker when the host is dropped.
    ticker_running: Arc<AtomicBool>,
}

impl Drop for ExtensionHost {
    fn drop(&mut self) {
        self.ticker_running.store(false, Ordering::Relaxed);
    }
}

impl ExtensionHost {
    /// A host with default resource caps.
    pub fn new() -> Result<Self> {
        Self::with_caps(ResourceCaps::default())
    }

    /// A host with explicit resource caps.
    pub fn with_caps(caps: ResourceCaps) -> Result<Self> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            // Both interruption mechanisms are compiled in; which one is armed
            // is decided per call by the ExecutionLimits.
            .epoch_interruption(true)
            .consume_fuel(true)
            // A guest must never see the host's stack; a modest limit also turns
            // deep recursion into a trap rather than a process abort.
            .max_wasm_stack(1024 * 1024)
            .cranelift_opt_level(wasmtime::OptLevel::Speed);

        let engine = Engine::new(&config).map_err(|e| WasmError::Compile {
            name: "<engine>".to_string(),
            source: anyhow_lite::Error::new(e),
        })?;

        // The epoch ticker: one thread advancing the engine's epoch counter, for
        // every store in the host. Without it, `set_epoch_deadline` never fires.
        let ticker_running = Arc::new(AtomicBool::new(true));
        {
            let engine = engine.clone();
            let running = Arc::clone(&ticker_running);
            std::thread::Builder::new()
                .name("nebula-wasm-epoch".to_string())
                .spawn(move || {
                    while running.load(Ordering::Relaxed) {
                        std::thread::sleep(EPOCH_TICK);
                        engine.increment_epoch();
                    }
                })
                .map_err(WasmError::Io)?;
        }

        Ok(Self { engine, world: WitWorld::current(), caps, ticker_running })
    }

    /// The world versions this host serves.
    pub fn world(&self) -> &WitWorld {
        &self.world
    }

    /// Compile a component.
    ///
    /// Checks, in order: that the bytes are a component rather than a core
    /// module, that the world version is one this host serves, and that every
    /// capability the component's imports imply was granted. A component asking
    /// for more than it was granted is refused here, before it can run.
    pub fn load(
        &self,
        name: &str,
        bytes: &[u8],
        granted: &BTreeSet<Capability>,
    ) -> Result<LoadedExtension> {
        if !is_component(bytes) {
            return Err(WasmError::NotAComponent(name.to_string()));
        }

        let imports = component_imports(bytes)?;
        let required = capabilities_from_imports(&imports);

        // The load-time check that makes the manifest meaningful: what the
        // binary imports, not what its manifest claims, decides what it needs.
        for capability in &required {
            if !granted.contains(capability) {
                return Err(WasmError::CapabilityNotGranted {
                    name: name.to_string(),
                    capability: capability.name().to_string(),
                });
            }
        }

        let world_version =
            world_version_from_imports(&imports).unwrap_or_else(|| self.world.latest().clone());
        if !self.world.supports(&world_version) {
            return Err(WasmError::IncompatibleWorld {
                name: name.to_string(),
                wanted: world_version.to_string(),
                available: self.world.latest().to_string(),
            });
        }

        let component = Component::new(&self.engine, bytes).map_err(|e| WasmError::Compile {
            name: name.to_string(),
            source: anyhow_lite::Error::new(e),
        })?;

        Ok(LoadedExtension { name: name.to_string(), component, required, world_version })
    }

    /// Compile a component from a file.
    pub fn load_file(
        &self,
        name: &str,
        path: impl AsRef<std::path::Path>,
        granted: &BTreeSet<Capability>,
    ) -> Result<LoadedExtension> {
        self.load(name, &std::fs::read(path)?, granted)
    }

    /// Instantiate a loaded component.
    pub async fn instantiate(
        &self,
        extension: &LoadedExtension,
        limits: ExecutionLimits,
    ) -> Result<ExtensionInstance> {
        let state = HostState {
            limits: StoreLimits::new(self.caps),
            granted: extension.required.clone(),
            name: extension.name.clone(),
        };

        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        arm_limits(&mut store, &limits);

        let linker: Linker<HostState> = Linker::new(&self.engine);
        // Host interfaces are added to the linker here in a full build; a
        // component importing an interface the linker does not provide fails to
        // instantiate, which is the WIT world doing its job.

        let instance =
            linker.instantiate_async(&mut store, &extension.component).await.map_err(|e| {
                WasmError::Instantiate { name: extension.name.clone(), detail: flatten(&e) }
            })?;

        Ok(ExtensionInstance { name: extension.name.clone(), store, instance, limits })
    }

    /// Load and instantiate in one step.
    pub async fn start(
        &self,
        name: &str,
        bytes: &[u8],
        granted: &BTreeSet<Capability>,
        limits: ExecutionLimits,
    ) -> Result<ExtensionInstance> {
        let loaded = self.load(name, bytes, granted)?;
        self.instantiate(&loaded, limits).await
    }
}

/// A running extension.
pub struct ExtensionInstance {
    name: String,
    store: Store<HostState>,
    instance: Instance,
    limits: ExecutionLimits,
}

impl ExtensionInstance {
    /// The extension's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the guest hit its memory cap.
    pub fn hit_memory_limit(&self) -> bool {
        self.store.data().hit_memory_limit()
    }

    /// Remaining instruction budget, if fuel is armed.
    pub fn fuel_remaining(&self) -> Option<u64> {
        self.store.get_fuel().ok()
    }

    /// Call an exported function.
    ///
    /// The limits are re-armed before every call: an epoch deadline is consumed
    /// by the call it bounds, and a second call with a stale deadline would trap
    /// immediately.
    pub async fn call(&mut self, export: &str, arguments: &[Val]) -> Result<Vec<Val>> {
        let function = self.instance.get_func(&mut self.store, export).ok_or_else(|| {
            WasmError::MissingExport { name: self.name.clone(), export: export.to_string() }
        })?;

        let result_count = function.ty(&self.store).results().len();
        let mut results = vec![Val::Bool(false); result_count];

        arm_limits(&mut self.store, &self.limits);

        match function.call_async(&mut self.store, arguments, &mut results).await {
            // Wasmtime 47 runs the component's `post-return` as part of the
            // call, so there is nothing further to do on success.
            Ok(()) => Ok(results),
            Err(error) => Err(self.classify(error)),
        }
    }

    /// Turn a Wasmtime error into the specific reason the guest stopped.
    ///
    /// "the extension trapped" is not an actionable message. Whether it ran out
    /// of instructions, blew its deadline, or asked for too much memory is what
    /// the user and the marketplace reviewer need to know.
    fn classify(&self, error: wasmtime::Error) -> WasmError {
        if self.store.data().hit_memory_limit() {
            return WasmError::OutOfMemory {
                name: self.name.clone(),
                limit: self.store.data().limits.caps().max_memory_bytes,
            };
        }
        if let Some(trap) = error.downcast_ref::<Trap>() {
            match trap {
                Trap::OutOfFuel => {
                    return WasmError::OutOfFuel {
                        name: self.name.clone(),
                        fuel: self.limits.fuel.unwrap_or(0),
                    };
                }
                Trap::Interrupt => {
                    return WasmError::DeadlineExceeded {
                        name: self.name.clone(),
                        millis: self.limits.epoch_deadline.as_millis() as u64,
                    };
                }
                _ => {}
            }
        }

        let detail = flatten(&error);
        // Wasmtime words fuel exhaustion differently across versions; matching
        // the text is a fallback for when the typed trap is not present.
        if detail.contains("all fuel consumed") {
            return WasmError::OutOfFuel {
                name: self.name.clone(),
                fuel: self.limits.fuel.unwrap_or(0),
            };
        }
        if detail.contains("epoch deadline") {
            return WasmError::DeadlineExceeded {
                name: self.name.clone(),
                millis: self.limits.epoch_deadline.as_millis() as u64,
            };
        }
        WasmError::Trap { name: self.name.clone(), detail }
    }
}

/// Arm the interruption mechanisms on a store.
fn arm_limits(store: &mut Store<HostState>, limits: &ExecutionLimits) {
    // Wasm traps immediately if this is not set before running.
    store.set_epoch_deadline(limits.epoch_ticks());
    match limits.fuel {
        Some(fuel) => {
            let _ = store.set_fuel(fuel);
        }
        None => {
            // Fuel is compiled in engine-wide, so a store that does not want it
            // is given effectively unlimited fuel rather than none — a store
            // with zero fuel traps on its first instruction.
            let _ = store.set_fuel(u64::MAX);
        }
    }
}

/// Whether `bytes` is a component rather than a core module.
///
/// Both start with the same magic; byte 8 of the version field distinguishes
/// them — layer 1 means component, layer 0 means core module.
pub fn is_component(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[..4] == [0x00, 0x61, 0x73, 0x6D] && bytes[6] == 0x01
}

/// The interface names a component imports.
pub fn component_imports(bytes: &[u8]) -> Result<Vec<String>> {
    use wasmparser::{Parser, Payload};

    let mut imports = Vec::new();
    // Only the outermost component's imports matter: a nested component's
    // imports are satisfied by its parent, not by the host.
    let mut depth = 0i32;

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| WasmError::Compile {
            name: "<component>".to_string(),
            source: anyhow_lite::Error::new(e),
        })?;
        match payload {
            Payload::ComponentImportSection(section) => {
                if depth != 1 {
                    continue;
                }
                for import in section {
                    let import = import.map_err(|e| WasmError::Compile {
                        name: "<component>".to_string(),
                        source: anyhow_lite::Error::new(e),
                    })?;
                    imports.push(import.name.0.to_string());
                }
            }
            Payload::ModuleSection { .. } => {}
            Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) => depth -= 1,
            _ => {}
        }
    }
    Ok(imports)
}

/// The world version a component targets, taken from its import names.
fn world_version_from_imports(imports: &[String]) -> Option<WorldVersion> {
    imports
        .iter()
        .filter(|import| import.starts_with("nebula:ide/"))
        .filter_map(|import| import.split('@').nth(1))
        .filter_map(WorldVersion::parse)
        .max()
}

/// Flatten an error's source chain into one message.
fn flatten(error: &wasmtime::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = std::error::Error::source(error.as_ref() as &dyn std::error::Error);
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A component exporting `run`, which returns 42.
    fn simple_component() -> Vec<u8> {
        wat::parse_str(
            r#"
            (component
              (core module $m
                (func (export "run") (result i32) i32.const 42)
              )
              (core instance $i (instantiate $m))
              (func (export "run") (result s32)
                (canon lift (core func $i "run")))
            )
            "#,
        )
        .expect("the test component should assemble")
    }

    /// A component whose `run` never returns.
    fn infinite_loop_component() -> Vec<u8> {
        wat::parse_str(
            r#"
            (component
              (core module $m
                (func (export "run") (result i32)
                  (loop $forever (br $forever))
                  i32.const 0
                )
              )
              (core instance $i (instantiate $m))
              (func (export "run") (result s32)
                (canon lift (core func $i "run")))
            )
            "#,
        )
        .expect("the test component should assemble")
    }

    /// A component that grows its memory as far as it is allowed to, and
    /// reports how many pages it obtained.
    fn memory_hog_component() -> Vec<u8> {
        wat::parse_str(
            r#"
            (component
              (core module $m
                (memory (export "mem") 1)
                (func (export "run") (result i32)
                  (local $grown i32)
                  (local $result i32)
                  (block $done
                    (loop $grow
                      ;; Ask for 16 more pages (1 MiB) at a time.
                      (local.set $result (memory.grow (i32.const 16)))
                      (br_if $done (i32.eq (local.get $result) (i32.const -1)))
                      (local.set $grown (i32.add (local.get $grown) (i32.const 16)))
                      ;; Stop after 4096 pages (256 MiB) so a host with no cap
                      ;; still terminates.
                      (br_if $done (i32.gt_u (local.get $grown) (i32.const 4096)))
                      (br $grow)
                    )
                  )
                  (local.get $grown)
                )
              )
              (core instance $i (instantiate $m))
              (func (export "run") (result s32)
                (canon lift (core func $i "run")))
            )
            "#,
        )
        .expect("the test component should assemble")
    }

    /// A core module, which is not a component.
    fn core_module() -> Vec<u8> {
        wat::parse_str(r#"(module (func (export "run") (result i32) i32.const 1))"#).unwrap()
    }

    fn no_capabilities() -> BTreeSet<Capability> {
        BTreeSet::new()
    }

    #[test]
    fn components_and_core_modules_are_distinguished() {
        assert!(is_component(&simple_component()));
        assert!(!is_component(&core_module()), "a core module is not a component");
        assert!(!is_component(b"not wasm at all"));
        assert!(!is_component(&[]));
    }

    #[tokio::test]
    async fn a_component_runs_and_returns_its_value() {
        let host = ExtensionHost::new().unwrap();
        let mut instance = host
            .start(
                "simple",
                &simple_component(),
                &no_capabilities(),
                ExecutionLimits::interactive(),
            )
            .await
            .unwrap();

        let results = instance.call("run", &[]).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], Val::S32(42));
    }

    #[tokio::test]
    async fn a_component_can_be_called_repeatedly() {
        // The epoch deadline is consumed by the call it bounds, so a second call
        // fails unless the limits are re-armed.
        let host = ExtensionHost::new().unwrap();
        let mut instance = host
            .start(
                "simple",
                &simple_component(),
                &no_capabilities(),
                ExecutionLimits::interactive(),
            )
            .await
            .unwrap();

        for _ in 0..10 {
            assert_eq!(instance.call("run", &[]).await.unwrap()[0], Val::S32(42));
        }
    }

    #[tokio::test]
    async fn a_core_module_is_rejected_with_a_useful_message() {
        let host = ExtensionHost::new().unwrap();
        let result = host.load("legacy", &core_module(), &no_capabilities());
        let err = match result {
            Err(err) => err,
            Ok(_) => panic!("a core module must not load as a component"),
        };

        assert!(matches!(err, WasmError::NotAComponent(_)));
        assert!(
            err.to_string().contains("wasm32-wasip2"),
            "the error should say how to fix it: {err}"
        );
    }

    #[tokio::test]
    async fn a_missing_export_is_named() {
        let host = ExtensionHost::new().unwrap();
        let mut instance = host
            .start(
                "simple",
                &simple_component(),
                &no_capabilities(),
                ExecutionLimits::interactive(),
            )
            .await
            .unwrap();

        let err = instance.call("does_not_exist", &[]).await.unwrap_err();
        assert!(matches!(err, WasmError::MissingExport { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn an_infinite_loop_is_stopped_by_the_epoch_deadline() {
        // The load-bearing property: an extension cannot freeze the editor.
        let host = ExtensionHost::new().unwrap();
        let limits = ExecutionLimits::interactive().deadline(Duration::from_millis(50));
        let mut instance = host
            .start("spinner", &infinite_loop_component(), &no_capabilities(), limits)
            .await
            .unwrap();

        let started = Instant::now();
        let err = instance.call("run", &[]).await.unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            matches!(err, WasmError::DeadlineExceeded { .. }),
            "expected a deadline error, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the deadline did not actually interrupt the guest ({elapsed:?})"
        );
    }

    #[tokio::test]
    async fn an_infinite_loop_is_stopped_by_running_out_of_fuel() {
        let host = ExtensionHost::new().unwrap();
        // A deterministic budget, with a deadline long enough that fuel is what
        // stops it rather than the clock.
        let limits = ExecutionLimits::deterministic(100_000).deadline(Duration::from_secs(30));
        let mut instance = host
            .start("spinner", &infinite_loop_component(), &no_capabilities(), limits)
            .await
            .unwrap();

        let err = instance.call("run", &[]).await.unwrap_err();
        assert!(matches!(err, WasmError::OutOfFuel { .. }), "expected a fuel error, got {err:?}");
    }

    #[tokio::test]
    async fn fuel_exhaustion_is_deterministic() {
        // The property that makes fuel worth its cost: the same component with
        // the same budget fails at the same point every time.
        let host = ExtensionHost::new().unwrap();
        let mut outcomes = Vec::new();

        for _ in 0..3 {
            let limits = ExecutionLimits::deterministic(50_000).deadline(Duration::from_secs(30));
            let mut instance = host
                .start("spinner", &infinite_loop_component(), &no_capabilities(), limits)
                .await
                .unwrap();
            outcomes
                .push(matches!(instance.call("run", &[]).await, Err(WasmError::OutOfFuel { .. })));
        }
        assert_eq!(outcomes, vec![true, true, true]);
    }

    #[tokio::test]
    async fn a_normal_call_does_not_exhaust_a_reasonable_budget() {
        let host = ExtensionHost::new().unwrap();
        let limits = ExecutionLimits::deterministic(1_000_000);
        let mut instance =
            host.start("simple", &simple_component(), &no_capabilities(), limits).await.unwrap();

        assert_eq!(instance.call("run", &[]).await.unwrap()[0], Val::S32(42));
        assert!(instance.fuel_remaining().is_some_and(|fuel| fuel > 0));
    }

    #[tokio::test]
    async fn memory_growth_is_capped() {
        // 8 MiB cap against a guest that asks for 256 MiB.
        let caps = ResourceCaps::default().memory(8 * 1024 * 1024);
        let host = ExtensionHost::with_caps(caps).unwrap();
        let limits = ExecutionLimits::background().deadline(Duration::from_secs(10));

        let mut instance =
            host.start("hog", &memory_hog_component(), &no_capabilities(), limits).await.unwrap();

        let results = instance.call("run", &[]).await.unwrap();
        let pages = match results[0] {
            Val::S32(pages) => pages,
            ref other => panic!("expected a page count, got {other:?}"),
        };

        // A page is 64 KiB, so an 8 MiB cap is 128 pages.
        assert!(pages <= 128, "the guest obtained {pages} pages against a 128-page cap");
        assert!(
            instance.hit_memory_limit(),
            "the refusal should be recorded so the host can explain it"
        );
    }

    #[tokio::test]
    async fn a_generous_memory_cap_lets_a_guest_allocate() {
        let caps = ResourceCaps::default().memory(128 * 1024 * 1024);
        let host = ExtensionHost::with_caps(caps).unwrap();
        let limits = ExecutionLimits::background().deadline(Duration::from_secs(10));

        let mut instance =
            host.start("hog", &memory_hog_component(), &no_capabilities(), limits).await.unwrap();

        let results = instance.call("run", &[]).await.unwrap();
        let pages = match results[0] {
            Val::S32(pages) => pages,
            ref other => panic!("expected a page count, got {other:?}"),
        };
        assert!(pages > 128, "a 128 MiB cap should permit more than 128 pages, got {pages}");
    }

    #[tokio::test]
    async fn several_extensions_run_independently() {
        let host = ExtensionHost::new().unwrap();
        let mut first = host
            .start("a", &simple_component(), &no_capabilities(), ExecutionLimits::interactive())
            .await
            .unwrap();
        let mut second = host
            .start("b", &simple_component(), &no_capabilities(), ExecutionLimits::interactive())
            .await
            .unwrap();

        assert_eq!(first.call("run", &[]).await.unwrap()[0], Val::S32(42));
        assert_eq!(second.call("run", &[]).await.unwrap()[0], Val::S32(42));

        // One extension timing out must not affect another.
        let limits = ExecutionLimits::interactive().deadline(Duration::from_millis(30));
        let mut spinner = host
            .start("spinner", &infinite_loop_component(), &no_capabilities(), limits)
            .await
            .unwrap();
        assert!(spinner.call("run", &[]).await.is_err());
        assert_eq!(
            first.call("run", &[]).await.unwrap()[0],
            Val::S32(42),
            "a neighbour's trap must not disturb this instance"
        );
    }

    #[test]
    fn imports_are_read_from_the_binary() {
        // The simple component imports nothing, which is itself the assertion:
        // capabilities are derived from what the binary actually imports.
        let imports = component_imports(&simple_component()).unwrap();
        assert!(imports.is_empty(), "got {imports:?}");
        assert!(capabilities_from_imports(&imports).is_empty());
    }

    #[test]
    fn malformed_bytes_are_reported_rather_than_panicking() {
        let mut garbage = simple_component();
        garbage.truncate(garbage.len() / 2);
        assert!(component_imports(&garbage).is_err());
    }

    #[tokio::test]
    async fn a_host_can_be_created_and_dropped_repeatedly() {
        // Each host spawns an epoch ticker thread; leaking one per host would
        // accumulate threads over a long editing session.
        for _ in 0..8 {
            let host = ExtensionHost::new().unwrap();
            let mut instance = host
                .start(
                    "simple",
                    &simple_component(),
                    &no_capabilities(),
                    ExecutionLimits::interactive(),
                )
                .await
                .unwrap();
            assert_eq!(instance.call("run", &[]).await.unwrap()[0], Val::S32(42));
        }
    }
}
