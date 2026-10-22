// Cases beyond the tsc/showConfig baselines. The expected text is the output of Go's ConvertToTSConfig and
// json.MarshalIndentWrite at the pinned commit for the same config text.

use tsrs_core::json::marshal_indent;

use crate::showconfig::convert_to_tsconfig;
use crate::tsoptionstest::get_parsed_command_line;

fn show_config(config_text: &str) -> String {
    let parsed = get_parsed_command_line(config_text, &[("/project/a.ts", "")], "/project", true);
    marshal_indent(&convert_to_tsconfig(&parsed, "/project/tsconfig.json"), "", "    ").unwrap()
}

#[test]
fn lib_and_enum_aliases_print_their_first_name() {
    let actual = show_config(
        r#"{"compilerOptions":{"lib":["es6","DOM","es2015.core"],"target":"es6","moduleResolution":"node10","module":"commonjs"}}"#,
    );
    assert_eq!(
        actual,
        r#"{
    "compilerOptions": {
        "lib": [
            "es6",
            "dom",
            "es2015.core"
        ],
        "module": "commonjs",
        "moduleResolution": "node",
        "target": "es6",
        "useDefineForClassFields": false
    },
    "files": [
        "./a.ts"
    ]
}"#
    );
}

#[test]
fn file_paths_are_relative_to_the_config_and_other_strings_are_not() {
    let actual = show_config(
        r#"{"compilerOptions":{"typeRoots":["./types","../shared/types"],"rootDirs":["src","gen"],"outFile":"out/all.js","tsBuildInfoFile":"build/info.tsbuildinfo","sourceRoot":"src/root","types":[],"customConditions":["a","b"]}}"#,
    );
    assert_eq!(
        actual,
        r#"{
    "compilerOptions": {
        "customConditions": [
            "a",
            "b"
        ],
        "rootDirs": [
            "./src",
            "./gen"
        ],
        "sourceRoot": "src/root",
        "tsBuildInfoFile": "./build/info.tsbuildinfo",
        "typeRoots": [
            "./types",
            "../shared/types"
        ],
        "types": [],
        "outFile": "./out/all.js"
    },
    "files": [
        "./a.ts"
    ]
}"#
    );
}

#[test]
fn references_mark_circular_ones_and_compile_on_save_prints_when_true() {
    let actual = show_config(
        r#"{"references":[{"path":"./x"},{"path":"../y","circular":true}],"compilerOptions":{"composite":true},"compileOnSave":true}"#,
    );
    assert_eq!(
        actual,
        r#"{
    "compilerOptions": {
        "composite": true,
        "declaration": true,
        "incremental": true
    },
    "references": [
        {
            "path": "./x"
        },
        {
            "path": "../y",
            "circular": true
        }
    ],
    "files": [
        "./a.ts"
    ],
    "compileOnSave": true
}"#
    );
}

#[test]
fn implied_options_equal_to_the_default_are_left_out() {
    assert_eq!(
        show_config(r#"{"compilerOptions":{"target":"es5"}}"#),
        r#"{
    "compilerOptions": {
        "target": "es5",
        "module": "commonjs",
        "useDefineForClassFields": false
    },
    "files": [
        "./a.ts"
    ]
}"#
    );
    assert_eq!(
        show_config(r#"{"compilerOptions":{"target":"esnext"}}"#),
        r#"{
    "compilerOptions": {
        "target": "esnext",
        "module": "esnext"
    },
    "files": [
        "./a.ts"
    ]
}"#
    );
}

#[test]
fn empty_include_and_exclude_print_nothing() {
    assert_eq!(
        show_config(r#"{"compilerOptions":{},"include":[],"exclude":[]}"#),
        r#"{
    "compilerOptions": {}
}"#
    );
}

#[test]
fn false_booleans_numbers_plugins_and_enums_print() {
    let actual = show_config(
        r#"{"compilerOptions":{"jsx":"react-jsx","newLine":"lf","strict":false,"noEmit":true,"maxNodeModuleJsDepth":2,"plugins":[{"name":"x"}],"watchFile":"fixedpollinginterval"}}"#,
    );
    assert_eq!(
        actual,
        r#"{
    "compilerOptions": {
        "jsx": "react-jsx",
        "newLine": "lf",
        "noEmit": true,
        "plugins": [
            {
                "name": "x"
            }
        ],
        "strict": false,
        "maxNodeModuleJsDepth": 2
    },
    "files": [
        "./a.ts"
    ]
}"#
    );
}

#[test]
fn the_default_include_is_left_out() {
    assert_eq!(
        show_config(r#"{"include":["**/*"],"compilerOptions":{"allowJs":true}}"#),
        r#"{
    "compilerOptions": {
        "allowJs": true
    },
    "files": [
        "./a.ts"
    ]
}"#
    );
}
