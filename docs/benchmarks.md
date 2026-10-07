# Benchmark harness

Opt-in benchmark evidence is produced by the four `task bench:*` targets. `task test:benchmarks` runs deterministic schema/statistics and workload invariant tests only. Core and intent headline runs use a default allocator; `:allocations` builds are separate Rust allocation-counting passes and are not headline timing. Evidence is written under unique `target/benchmarks/{headline|allocations}/<run-id>/` directories. A schema-3 status starts `preparing`, moves to `measuring`, and is completed as `validated` only after boundary verification; interrupted or rejected runs are not completed evidence.

## Provenance contract

The benchmark tasks opt into a build-time provenance stamp. The normal WM build remains independent of Git inspection, and the existing Windows resource compilation still runs normally. Taskfile targets preserve operator overrides in `GLAZEWM_OPERATOR_*` values before Cargo injects build-script variables. Runtime boundaries compare those supported operator settings; the stamp separately records Cargo's resolved compiler (`RUSTC`), effective encoded flags (`CARGO_ENCODED_RUSTFLAGS`), actual `PROFILE`/`HOST`/`TARGET`/`OPT_LEVEL`/`DEBUG`, features and tool versions. Cargo removes/transforms `RUSTFLAGS` for build scripts, so raw build-script environment values are not mistaken for the operator inputs. Covered inputs include manifests, lockfile, package source/build inputs, resources/assets, repository Cargo configuration (ancestor and Cargo-home config files are hashed too, present or absent), `Taskfile.yml`, this document, and toolchain selection. Staged-only, unstaged, untracked and deleted inputs are included relative to HEAD. The harness verifies the stamp before fixture preparation and again before publication; it hashes and retains the actual executable before measurement. Failures to read inputs, retain/hash the executable, match the expected allocator mode, or verify inputs reject the run.

`task test:benchmark-provenance-smoke` runs real release Cargo builds with unset overrides, nonempty `RUSTFLAGS`, and a persistent supported-setting mismatch. Smoke artifacts live under `target/benchmarks/smoke/`, end as `smoke-validated` or `rejected`, and contain no measurement manifest or samples; they are not headline evidence.

Cargo re-runs the stamp build script when a present Cargo config file or anything under the repository's `.cargo`, `packages` or `resources/assets` changes. It does not watch absent ancestor or Cargo-home config candidates, because Cargo treats a missing watched path as permanently dirty and walks watched directories recursively; a config file added there after the build is still rejected by the runtime input verification, and the operator rebuilds after touching a covered file.

This is conditional provenance, not compiler-input proof. Operators must make no covered edits while Cargo compiles or while a measurement runs. Persistent boundary changes are rejected; transient edit-and-revert cannot be detected. Registry source caches, compiler/linker binaries, SDKs/frameworks and normal Cargo dependency integrity remain trusted; they are not frozen or content-attested. The stamp does not prove those tool inputs immutable.

## Workloads and boundaries

The intent suite has all twelve cases: focus, workspace switch, checked creation, and resize-command burst at N=10/50/100. Each case uses 1,000 warmups and three repetitions of 20,000 samples. Fixture setup, preparation, validation and cleanup are outside timing; raw samples and summaries remain distinct. The allocation pass is separate from headline timing. Resize measures a 256-command batch, not one-event latency.

- Focus: production command through returned pending intent.
- Workspace switch: production command through displayed/recent workspace and pending intent.
- Resize: one batch of 256 production resize commands through pending intent.
- Checked creation: checked property/monitor seam, N-1 to N. A new managed model and UUID are included in timing; preparation and output validation/cleanup are outside timing.

Creation reuses one `NativeWindow::mock()` identity per harness invocation. On macOS construction makes two AX objects; clones share those objects, which remain retained until process exit because the stopped dispatcher prevents disposal. Retention is constant per process, not per sample. No Rust allocation count is an AX-resource measurement.

Core layout and reconciliation workloads remain separate. Fixture construction/UUID generation are outside core timing. Native acquisition, manage rules, synchronization, rendering, foreign allocator counts and visual completion are excluded. Allocation counts describe Rust `System` calls only. Analytical traffic estimates are illustrative, not a complete physical floor or a performance claim.
