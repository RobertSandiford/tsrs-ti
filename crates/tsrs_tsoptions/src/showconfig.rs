// Port of Go's tsoptions/showconfig.go: the tsconfig that `--showConfig` prints.

use tsrs_core::collections::OrderedMap;
use tsrs_core::json::{JsonMap, Value};
use tsrs_core::tspath::{self, ComparePathsOptions};
use tsrs_core::{CompilerOptions, Tristate};
use tsrs_diagnostics as diagnostics;

use crate::commandlineoption::{CommandLineOption, CommandLineOptionKind, CompilerOptionsValue};
use crate::parsedcommandline::ParsedCommandLine;
use crate::parsinghelpers::for_each_compiler_option_value;
use crate::tsconfigparsing::COMMAND_LINE_COMPILER_OPTIONS_MAP;

const DEFAULT_INCLUDE_SPEC: &str = "**/*";

// An option whose effective value follows from other options (TypeScript's computedOptions in convertToTSConfig).
struct ImpliedOption {
    name: &'static str,
    // Option names this option's value depends on.
    dependencies: &'static [&'static str],
    compute: fn(&CompilerOptions) -> CompilerOptionsValue,
}

macro_rules! implied {
    ($name:literal, [$($dep:literal),*], $variant:ident, $getter:ident) => {
        ImpliedOption {
            name: $name,
            dependencies: &[$($dep),*],
            compute: |o| CompilerOptionsValue::$variant(o.$getter()),
        }
    };
}

static IMPLIED_OPTIONS: [ImpliedOption; 14] = [
    implied!("module", ["target"], ModuleKind, get_emit_module_kind),
    implied!(
        "moduleResolution",
        ["module", "target"],
        ModuleResolutionKind,
        get_module_resolution_kind
    ),
    implied!(
        "moduleDetection",
        ["module", "target"],
        ModuleDetectionKind,
        get_emit_module_detection_kind
    ),
    implied!("isolatedModules", ["verbatimModuleSyntax"], Bool, get_isolated_modules),
    implied!(
        "preserveConstEnums",
        ["isolatedModules", "verbatimModuleSyntax"],
        Bool,
        should_preserve_const_enums
    ),
    implied!("declaration", ["composite"], Bool, get_emit_declarations),
    implied!(
        "declarationMap",
        ["declaration", "composite"],
        Bool,
        get_are_declaration_maps_enabled
    ),
    implied!("incremental", ["composite"], Bool, is_incremental),
    implied!(
        "useDefineForClassFields",
        ["target", "module"],
        Bool,
        get_use_define_for_class_fields
    ),
    implied!(
        "resolvePackageJsonExports",
        ["moduleResolution", "module", "target"],
        Bool,
        get_resolve_package_json_exports
    ),
    implied!(
        "resolvePackageJsonImports",
        ["moduleResolution", "resolvePackageJsonExports", "module", "target"],
        Bool,
        get_resolve_package_json_imports
    ),
    implied!(
        "resolveJsonModule",
        ["moduleResolution", "module", "target"],
        Bool,
        get_resolve_json_module
    ),
    implied!("allowJs", ["checkJs"], Bool, get_allow_js),
    implied!(
        "allowImportingTsExtensions",
        ["rewriteRelativeImportExtensions"],
        Bool,
        get_allow_importing_ts_extensions
    ),
];

// File listings are left out of --showConfig despite their Compiler Diagnostics category (`showConfig: false`).
const SHOW_CONFIG_FALSE_OPTIONS: [&str; 2] = ["listEmittedFiles", "listFiles"];

// Options that only make sense on the command line.
const COMMAND_LINE_ONLY_OPTIONS: [&str; 10] = [
    "showConfig",
    "configFile",
    "configFilePath",
    "help",
    "init",
    "listFilesOnly",
    "listEmittedFiles",
    "project",
    "build",
    "version",
];

// ConvertToTSConfig generates the complete tsconfig for --showConfig output, matching TypeScript's convertToTSConfig.
pub fn convert_to_tsconfig(
    config_parse_result: &ParsedCommandLine,
    config_file_name: &str,
) -> Value {
    let config_file_name =
        if config_file_name.is_empty() { "tsconfig.json" } else { config_file_name };
    let current_directory = config_parse_result.get_current_directory();
    let normalized_config_path =
        tspath::get_normalized_absolute_path(config_file_name, current_directory);
    let compare_paths_options = ComparePathsOptions {
        current_directory: current_directory.to_string(),
        use_case_sensitive_file_names: config_parse_result.use_case_sensitive_file_names(),
    };

    // All resolved files, relative to the config file.
    let files: Vec<Value> = config_parse_result
        .file_names()
        .iter()
        .map(|f| {
            let normalized_file_path = tspath::get_normalized_absolute_path(f, current_directory);
            Value::String(tspath::get_relative_path_from_file(
                &normalized_config_path,
                &normalized_file_path,
                &compare_paths_options,
            ))
        })
        .collect();

    let compiler_options = config_parse_result.compiler_options().unwrap();
    let mut option_map = serialize_compiler_options(
        &compiler_options,
        &normalized_config_path,
        &compare_paths_options,
    );
    for name in COMMAND_LINE_ONLY_OPTIONS {
        option_map.shift_remove(name);
    }

    // Options implied by explicitly set ones, such as moduleResolution by module or useDefineForClassFields by target.
    add_implied_options(&mut option_map, &compiler_options);

    let mut config = JsonMap::new();
    config.insert("compilerOptions".to_string(), Value::Object(option_map));

    let references = config_parse_result.project_references();
    if !references.is_empty() {
        let references = references
            .iter()
            .map(|r| {
                let mut reference = JsonMap::new();
                reference.insert("path".to_string(), Value::String(r.original_path.clone()));
                if r.circular {
                    reference.insert("circular".to_string(), Value::Bool(true));
                }
                Value::Object(reference)
            })
            .collect();
        config.insert("references".to_string(), Value::Array(references));
    }

    if !files.is_empty() {
        config.insert("files".to_string(), Value::Array(files));
    }

    if let Some(config_file) = config_parse_result.config_file {
        if let Some(specs) = config_file.config_file_specs.borrow().as_ref() {
            let include = filter_same_as_default_include(&specs.validated_include_specs);
            if !include.is_empty() {
                config.insert("include".to_string(), strings(include));
            }
            if !specs.validated_exclude_specs.is_empty() {
                config.insert("exclude".to_string(), strings(&specs.validated_exclude_specs));
            }
        }
    }

    if config_parse_result.compile_on_save == Some(true) {
        config.insert("compileOnSave".to_string(), Value::Bool(true));
    }

    Value::Object(config)
}

fn strings(values: &[String]) -> Value {
    Value::Array(values.iter().map(|s| Value::String(s.clone())).collect())
}

// Returns nothing for the default include spec ["**/*"].
fn filter_same_as_default_include(specs: &[String]) -> &[String] {
    if specs.len() == 1 && specs[0] == DEFAULT_INCLUDE_SPEC { &[] } else { specs }
}

// The name of the first enum map entry holding value.
fn name_of_compiler_option_value(
    value: &CompilerOptionsValue,
    enum_map: &OrderedMap<&'static str, CompilerOptionsValue>,
) -> Option<&'static str> {
    enum_map.iter().find(|(_, v)| *v == value).map(|(k, _)| *k)
}

// Converts the compiler options to an ordered map with enums as their names and paths relative to the config file.
fn serialize_compiler_options(
    options: &CompilerOptions,
    config_file_path: &str,
    compare_paths_options: &ComparePathsOptions,
) -> JsonMap {
    let mut result = JsonMap::new();
    let config_dir = tspath::get_directory_path(config_file_path);
    let relative_to_config = |path: &str| {
        let abs_path = tspath::get_normalized_absolute_path(path, &config_dir);
        tspath::get_relative_path_from_file(config_file_path, &abs_path, compare_paths_options)
    };

    for_each_compiler_option_value(
        options,
        |option| !is_skipped_category(option) && !SHOW_CONFIG_FALSE_OPTIONS.contains(&option.name),
        |option, value, _| {
            if value.is_zero() {
                return false;
            }
            let value = value.interface();

            if let Some(enum_map) = option.enum_map() {
                if let Some(name) = name_of_compiler_option_value(&value, enum_map) {
                    result.insert(option.name.to_string(), Value::String(name.to_string()));
                }
                return false;
            }

            let serialized = match (option.kind, &value) {
                (CommandLineOptionKind::List, CompilerOptionsValue::StringArray(strs)) => {
                    let elem = option.elements();
                    if elem.is_some_and(|e| e.is_file_path) {
                        Value::Array(
                            strs.iter().map(|s| Value::String(relative_to_config(s))).collect(),
                        )
                    } else if let Some(elem_map) = elem.and_then(CommandLineOption::enum_map) {
                        // lib values are stored as the d.ts file name; find the original key.
                        Value::Array(
                            strs.iter()
                                .map(|s| {
                                    let stored = CompilerOptionsValue::String(s.clone());
                                    let name = name_of_compiler_option_value(&stored, elem_map);
                                    Value::String(name.map_or_else(|| s.clone(), str::to_string))
                                })
                                .collect(),
                        )
                    } else {
                        options_value_to_json(&value)
                    }
                }
                (CommandLineOptionKind::String, CompilerOptionsValue::String(s))
                    if option.is_file_path && !s.is_empty() =>
                {
                    Value::String(relative_to_config(s))
                }
                _ => options_value_to_json(&value),
            };
            result.insert(option.name.to_string(), serialized);
            false
        },
    );
    result
}

fn is_skipped_category(option: &CommandLineOption) -> bool {
    option.category.is_some_and(|category| {
        std::ptr::eq(category, &raw const diagnostics::Command_line_Options)
            || std::ptr::eq(category, &raw const diagnostics::Output_Formatting)
    })
}

fn options_value_to_json(value: &CompilerOptionsValue) -> Value {
    match value {
        CompilerOptionsValue::Bool(b) => Value::Bool(*b),
        CompilerOptionsValue::Tristate(Tristate::True) => Value::Bool(true),
        CompilerOptionsValue::Tristate(Tristate::False) => Value::Bool(false),
        CompilerOptionsValue::Float(f) => Value::Number(*f),
        CompilerOptionsValue::Int(i) => Value::Integer(*i),
        CompilerOptionsValue::String(s) => Value::String(s.clone()),
        CompilerOptionsValue::StringArray(strs) => strings(strs),
        CompilerOptionsValue::Array(items) => {
            Value::Array(items.iter().map(options_value_to_json).collect())
        }
        CompilerOptionsValue::Object(m) => {
            Value::Object(m.iter().map(|(k, v)| (k.clone(), options_value_to_json(v))).collect())
        }
        _ => Value::Null,
    }
}

// Adds the options implied by explicitly set ones, mirroring TypeScript's computedOptions.
// For example, module: nodenext implies moduleResolution: nodenext.
fn add_implied_options(option_map: &mut JsonMap, options: &CompilerOptions) {
    let provided: Vec<String> = option_map.keys().map(str::to_string).collect();
    let default_options = CompilerOptions::default();

    for entry in &IMPLIED_OPTIONS {
        let Some(option) = COMMAND_LINE_COMPILER_OPTIONS_MAP.get(entry.name) else {
            continue;
        };
        if provided.iter().any(|p| p == option.name) {
            continue;
        }
        if !entry.dependencies.iter().any(|dep| {
            COMMAND_LINE_COMPILER_OPTIONS_MAP
                .get(dep)
                .is_some_and(|dep_option| provided.iter().any(|p| p == dep_option.name))
        }) {
            continue;
        }

        // A value equal to the one for empty options adds no information.
        let implied = (entry.compute)(options);
        if implied == (entry.compute)(&default_options) {
            continue;
        }
        if let Some(serialized) = serialize_implied_option_value(option, &implied) {
            option_map.insert(option.name.to_string(), serialized);
        }
    }
}

fn serialize_implied_option_value(
    option: &CommandLineOption,
    value: &CompilerOptionsValue,
) -> Option<Value> {
    if let Some(enum_map) = option.enum_map() {
        return name_of_compiler_option_value(value, enum_map)
            .map(|name| Value::String(name.to_string()));
    }
    match value {
        CompilerOptionsValue::Null => None,
        CompilerOptionsValue::Tristate(Tristate::Unknown) => None,
        value => Some(options_value_to_json(value)),
    }
}
