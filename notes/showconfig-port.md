# showconfig-port: plan for `--showConfig`

Written by Claude Opus 5.5 on 2026-10-21, on branch `show-config` (first at 2e6fb88c, now based on main d53a2a2a). This is a plan: nothing in it is
implemented. Go reference: the pinned commit `b85298b6` (`Cargo.toml` `[workspace.metadata.typescript]`), read with
`git show` from a local TypeScript checkout. Upstream main reworked the feature after the pin (#64457, generated option
definitions, 4 more baselines); this plan ports the pinned version.

## Where it stands

- `tsrs --showConfig` exits 5 with "--showConfig is not supported by tsrs." (`execute.rs:315`).
- The other check is already ported: with no tsconfig, `--showConfig` reports TS5081 "Cannot find a tsconfig.json
  file at the current directory" (`execute.rs:248`), as Go does.
- Every input the converter reads exists: `ParsedCommandLine::file_names`, `project_references` (with
  `original_path` and `circular`), `compile_on_save`, and the config file's `validated_include_specs` /
  `validated_exclude_specs` (`tsconfigparsing.rs:140`, `pub(crate)`, so the converter belongs in `tsrs_tsoptions`).
- The option table has what the serializer needs: `enum_map()`, `is_file_path`, `elements()` and `category`
  (`commandlineoption.rs`).
- The 14 getters the implied-options step calls exist in `tsrs_core/src/compileroptions.rs` (14 of 14 found by name).
- `for_each_compiler_options_field!` (`parsinghelpers.rs`) lists the fields in Go struct order with their JSON names;
  `gojson::compiler_options_to_go_json` uses it to emit options in that order with zero values left out. That order is
  the key order of `--showConfig` output (see the paths-and-more baseline below: `target` comes before `baseUrl`).
- `tsrs_core::json::marshal_indent` is the indenting printer.

## The Go code to port

| Go file | Lines | Notes |
| --- | ---: | --- |
| `tsoptions/showconfig.go` | 344 | the converter; all of it |
| `execute/tsc.go` | 4 | `showConfig()`: `ConvertToTSConfig`, then `json.MarshalIndentWrite(w, cfg, "", "    ")`; exit `Success` |

`ConvertToTSConfig(parsed, configFileName)`:

1. `configFileName` defaults to `tsconfig.json`; paths are made relative to it with `GetRelativePathFromFile`.
2. `files`: every resolved file name, relative to the config (`./src/index.ts`).
3. `compilerOptions`, by reflection over the `CompilerOptions` fields in struct order:
   - skip fields with no option declaration, options in the `Command_line_Options` or `Output_Formatting` categories,
     and zero values;
   - enums: number to name, the first key in the option's enum map with that value (so map order decides between
     aliases such as `node` / `node10`);
   - file-path strings and file-path lists: relative to the config;
   - `lib` (a list whose elements have an enum map): stored file name back to the first matching key, else unchanged;
   - booleans: `true` or `false` (a set `false` is printed);
   - everything else unchanged. `paths` is an object, so it is printed as written, not made relative
     (paths-and-more and `configDir-template-showConfig` show this).
4. Delete `showConfig`, `configFile`, `configFilePath`, `help`, `init`, `listFilesOnly`, `listEmittedFiles`,
   `project`, `build`, `version`.
5. Implied options (`impliedOptions`, 14 entries, in table order, appended after the explicit ones): add one when it
   was not given, at least one of its direct dependencies was given, and its computed value differs from the value
   computed on empty options. `module: nodenext` gives `moduleResolution: nodenext` and `moduleDetection: force`.
6. `references` (`path` = original path, `circular` only when true), `files`, `include` (dropped when it is exactly
   the default `**/*`), `exclude`, `compileOnSave` (only when true). Struct order, `omitzero`.

## Baselines

The 17 `tsc/showConfig` scenarios and `tsc/extends/configDir-template-showConfig`, classified by their output at the
pin:

| Output | Scenarios |
| --- | --- |
| JSON | `compileOnSave-and-more`, `exclude-and-outDir`, `exclude`, `files-and-include`, `files-options`, `include-filtering-files`, `paths-and-more`, `references`, `transitively-implied-options`, `configDir-template-showConfig` (10) |
| TS5081, no tsconfig | `Default-initialized-TSConfig`, `advanced-options`, `boolean-value-compiler-options`, `enum-value-compiler-options`, `list-compiler-options`, `list-compiler-options-with-enum-value` (6) |
| an option error first | `incorrect-compiler-option` (TS5023), `incorrect-compiler-option-value` (TS6046) (2) |

The 8 that error out probably pass today; that is unchecked, because this checkout has no `ts-ref/` and no scenario
dump. No baseline prints `lib`, an enum list, or a file-path list option, so those paths need unit tests.

## Steps

### 1. Reference and current numbers

- Set up `ts-ref/` at the pin as `CONTRIBUTING.md:55` describes.
- Run `tools/oracle/tsctests/dump.sh` (Git Bash on Windows; it needs `patch` and Go). Point `GOCACHE` at a directory
  under `target/`, never the shared cache.
- `TSCTESTS_FILTER=showConfig cargo test --profile dev -p tsrs_execute tsctests -- --nocapture`, and the same for
  `configDir-template`. Record the pass/fail split as the before number.

### 2. Failing tests first

- The scenario run in step 1 is the red test for the 10 JSON baselines.
- Unit tests in `crates/tsrs_tsoptions/src/showconfig_test.rs` for what no baseline covers: `lib` (including `es6`
  versus `es2015`, which map to the same file), a file-path list (`typeRoots`, `rootDirs`), an enum alias,
  `"types": []` (an empty list given in the config), `circular` references, an implied option equal to its default
  (left out). Take the expected output from Go: a temporary `_test.go` inside `ts-ref/tsc/internal/tsoptions` calling
  `ConvertToTSConfig`, deleted after use.

### 3. The converter: `crates/tsrs_tsoptions/src/showconfig.rs`

- `convert_to_tsconfig(parsed: &ParsedCommandLine, config_file_name: &str) -> Value`, returning the ordered JSON
  object directly instead of Go's `TSConfig` struct.
- Replace the reflection walk with a `for_each_compiler_options_field!` arm that yields each field's JSON name and Go
  JSON value (reuse `GoJson::to_go_json`, which already returns `None` for zero values), then look the declaration up
  by JSON name and convert as in step 3 above. Go looks the declaration up by Go field name, case-insensitively; check
  that the JSON names give the same declarations (a test over the field list can assert each name resolves).
- `impliedOptions` as a static table of `(name, dependencies, fn(&CompilerOptions) -> Value)`; compare against the
  result on `CompilerOptions::default()`.
- About 300 lines plus tests, against Go's 344.

### 4. Wire it in

- `execute.rs:315`: replace `not_supported` with printing `marshal_indent(&convert_to_tsconfig(..), "", "    ")`
  and returning `ExitStatus::Success`.
- Update the port comment at `execute.rs:1`.

### 5. Docs

- `docs/COMPATIBILITY.md:59`: split the `--init`, `--showConfig` row; `--showConfig` becomes yes, citing the
  scenario pass count.
- `docs/DEFAULTS.md:36`, `docs/PORTING.md:7` and `:217`, `docs/USAGE.md:18`.
- `README.md:30` will then be wrong ("`--init` and `--showConfig` are not supported"). AGENTS.md asks for README
  summaries to match the table, but a README is edited only when Robert asks: flag it in the pull request.

### 6. Verify

- The scenario run from step 1: 18 of 18 expected. Lint as `AGENTS.md` says.
- Run the dev build and tsgo with `--showConfig` on a few real projects (`bench/projects.json`) and diff the output,
  on Windows too: relative paths across a drive letter are the case the Linux baselines can't show.

## Things to check while porting

- Go's `json.MarshalIndentWrite` (tsgo's own `json` package): whether it ends with a newline and whether it escapes
  `<`, `>` and `&`; `marshal_indent` must match both.
- Empty but present lists: Go's `omitzero` keeps a non-nil empty slice (`"types": []` in the configDir baseline). Check
  that `Option<Vec<_>>::to_go_json` gives `[]` for `Some(vec![])`, and whether `validated_exclude_specs` can be
  non-nil and empty in Go (it would print `"exclude": []`) where tsrs has a plain `Vec`.
- The implied-option comparison is Go `reflect.DeepEqual` on the getter results; Rust compares the values directly.

## Size and risk

About 450 Rust lines with tests, as `notes/upstream-gaps-2026-10-08.md:215` estimated (it cites 389 Go lines,
upstream main's count; the pin has 344). The work is an output path at the edge of the CLI: it reads the parsed
config and never touches the checker. Most of the effort goes into matching the baselines byte for byte.

## Implementation report

By Claude Sonnet 5.5, on 2026-10-21, on branch `show-config`. The plan above is implemented. Nothing is committed.

### What I built

- `crates/tsrs_tsoptions/src/showconfig.rs` (about 270 lines): `convert_to_tsconfig(&ParsedCommandLine, config_file_name)
  -> Value`, a port of `ConvertToTSConfig`. Go's reflection walk over `CompilerOptions` is
  `for_each_compiler_option_value`, which already existed for the build-info writer, so no new field table was needed.
  The implied-options table is a static array of 14 entries, each calling the existing getter.
- `crates/tsrs_execute/src/execute.rs`: `--showConfig` prints `marshal_indent(.., "", "    ")` and exits `Success`,
  replacing the `not_supported` branch. The no-tsconfig branch (TS5081) was already ported and is unchanged.
- `crates/tsrs_tsoptions/src/showconfig_test.rs`: 7 tests for what the baselines don't cover.
- Docs: `COMPATIBILITY.md` (split the `--init` / `--showConfig` row; `tsc` count 191 to 201 of 223), `DEFAULTS.md`,
  `PORTING.md` (two places), `USAGE.md`.

### How it was checked

- Reference: `ts-ref/` cloned from the local TypeScript checkout (`git clone --local`, so that repo's branch was not
  touched) at the pin, `dump.sh` run in Git Bash with a Go cache under `target/` (deleted afterwards, 1.7 GB).
- Before: the 18 scenarios gave 8 pass, 10 fail, as predicted. After: 18 of 18 pass. The 10 JSON baselines went green
  on the first run of the converter.
- The 7 unit tests also passed on their first run, so I broke the subject three ways and watched them fail: taking the
  last enum alias instead of the first (`lib` and enum-alias tests red), not making file-path lists relative
  (file-paths test red), and keeping implied options that equal the default (implied-options tests red). All restored.
- Expected text in the unit tests comes from a temporary Go test calling `ConvertToTSConfig` at the pin
  (written outside the repo, copied into `ts-ref`, deleted after). Go's output was identical to ours on all 8 cases
  (the 7 tests plus one HTML and `U+2028` escaping check: neither side escapes `<`, `>`, `&` or `U+2028`).
- Output has no trailing newline in Go or here; the baselines pass with it.
- `cargo clippy -p tsrs_tsoptions -p tsrs_execute --all-targets --config 'build.warnings="deny"'` is clean;
  `cargo fmt` applied; `cargo test -p tsrs_tsoptions -p tsrs_execute` passes (52 and 12 tests).
- Ran the dev `tsrs.exe --showConfig` by hand on a small project with `module: nodenext`: it prints
  `moduleResolution` and `moduleDetection`, a relative `outDir`, `files`, `include` and an absolute `exclude`, exit 0.

### Moved onto main

The work was done on top of the Windows watch branch, then moved onto main d53a2a2a (uncommitted, via stash; the
`COMPATIBILITY.md` and `PORTING.md` conflicts were resolved by keeping main's watch wording). On main alone, the
`tsrs_execute` tests and `cargo clippy` for that crate do not build on Windows: `ipc::pipe` is Unix-only
(`tsrs_contentmappertest`) and `tsc/statistics.rs:116` and `tsrs_vfs` have Windows-only lints. The watch branch fixes
all of these, and none is in the showConfig code. So after the move I re-ran only what builds: `tsrs_tsoptions` tests
(52 pass, including the 7 new ones), clippy with `--no-deps` on `tsrs_tsoptions` (clean), the dev `tsrs.exe`
(`--showConfig` output and exit 0 unchanged). The 18-of-18 scenario result and the `tsc` count of 201 of 223 were
measured before the move, with the watch branch's fixes present; the converter and wiring are unchanged since.

### Notes for whoever lands this

1. **Counts were measured on Windows.** The `tsctests` summary table prints `0 / 0` for every row on Windows (it
   matches paths with `/`, the result files use `\`); I counted the result files by hand: `tsc` 201 of 223 pass,
   `tsbuild` 189 of 192, `tscWatch` 39 of 42, `tsbuildWatch` 65 of 65. The 22 `tsc` failures are `--help`, `--init`,
   `--locale` and `--generateTrace` (16), plus 3 `forceConsistentCasingInFileNames`, 2 `ignoreConfig` and
   `incremental/internal-symbolname-in-tsbuildInfo` that I did not look into; the casing ones may be Windows-only. The COMPATIBILITY row says "measured on Windows". Re-measure on Linux before the
   pull request if you want the table to say otherwise. I did not fix the Windows summary bug.
2. **README is now wrong.** `README.md:30` still says `--init` and `--showConfig` are not supported. I left it alone
   because READMEs are edited only on request; AGENTS.md also asks that it match the table.
3. **Empty `include` / `exclude`.** tsrs stores them as plain `Vec`s; Go's `nil` versus empty slice made no difference
   in the one case I ran (`"include": [], "exclude": []` prints nothing in both), so the converter omits empty ones.
4. **Windows paths.** `exclude` (and `include` when given as absolute paths) print as the config stored them, with
   forward slashes and a drive letter, as in the Linux baselines' absolute paths. No Windows baseline exists to
   compare with, and I had no tsgo binary to diff against, so the Windows output rests on the path functions being
   Go ports.
5. **Not done:** a diff against real tsgo on real projects (step 6 of the plan); `tools/lint/source.py` was not
   usable (it fails on Windows, as before, with nothing about these files); no Linux or macOS run.
6. **Cleanup:** no background processes were started; `ts-ref/` stays (git-ignored) as the reference checkout.
