use std::{collections::HashMap, path::Path};

fn parse_abi(abi: &str) -> (Vec<(&str, i32)>, HashMap<&str, &str>) {
    // Grammar: [section] headers or key = non-negative-integer, with whitespace/comments.
    let mut section = "";
    let mut constants = Vec::new();
    let mut callback = HashMap::new();
    for (index, raw_line) in abi.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = &line[1..line.len() - 1];
            if name.is_empty()
                || !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                panic!("invalid ABI section at line {}: {raw_line}", index + 1);
            }
            section = name;
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("invalid ABI assignment at line {}: {raw_line}", index + 1));
        let key = key.trim();
        let value = value.trim();
        if key.is_empty()
            || !key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            panic!("invalid ABI key at line {}: {raw_line}", index + 1);
        }
        if section == "callback" {
            callback.insert(key, value);
            continue;
        }
        if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
            panic!(
                "ABI constant at line {} must be a non-negative integer: {raw_line}",
                index + 1
            );
        }
        let value = value
            .parse::<i32>()
            .unwrap_or_else(|_| panic!("ABI constant at line {} exceeds i32", index + 1));
        constants.push((key, value));
    }
    (constants, callback)
}

/// Escape one path component for MoonBit's symbol mangling.
///
/// `'_' -> "__"` first, then `'-' -> "_2d"`. The order matters: the other way
/// round would re-escape the underscore that `_2d` introduces. Mirrors
/// `escape_mangling_component` in `moonbit-bindings/build.py`.
fn escape_mangling_component(s: &str) -> String {
    s.replace('_', "__").replace('-', "_2d")
}

/// Compute the mangled symbol of the MoonBit callback from its module path and
/// function name, both declared in `abi.toml`'s `[callback]`.
///
/// Scheme: `_M0FP<N><len1><comp1><len2><comp2>…<fnlen><fn>`, where `N` is the
/// number of module components and every length is that of the *escaped* text.
/// The callback lives in the module's root package, so no package component
/// appears. For module `nakake/gpui-bindings` and name `dispatch_entry`:
///
///   components: ["nakake", "gpui_2dbindings"]
///   fn:         "dispatch_entry" -> "dispatch__entry" (15 chars)
///   -> _M0FP2 6nakake 15gpui_2dbindings 15dispatch__entry
///   -> _M0FP26nakake15gpui_2dbindings15dispatch__entry
///
/// The leading `_` is the ELF symbol prefix; on Mach-O the linker adds the
/// second one, so `#[link_name]` carries exactly one either way. Mirrors
/// `compute_callback_symbol` in `moonbit-bindings/build.py` — the two must
/// agree, which is why both read the same two fields.
fn mangled_callback_symbol(module: &str, name: &str) -> String {
    let components: Vec<String> = module.split('/').map(escape_mangling_component).collect();
    let mut out = format!("_M0FP{}", components.len());
    for comp in &components {
        out.push_str(&format!("{}{}", comp.len(), comp));
    }
    let function = escape_mangling_component(name);
    out.push_str(&format!("{}{}", function.len(), function));
    out
}

fn main() {
    cc::Build::new()
        .file("src/benchmark_signpost.c")
        .compile("md_editor_benchmark_signpost");
    // --- Shared Rust/MoonBit ABI ---
    println!("cargo:rerun-if-changed=abi.toml");
    let abi = std::fs::read_to_string("abi.toml").expect("read abi.toml");
    let (constants, callback) = parse_abi(&abi);
    let mut rust_constants =
        String::from("// Auto-generated from abi.toml by build.rs. Do not edit manually.\n\n");
    for (key, value) in constants {
        let rust_name = key.to_ascii_uppercase();
        rust_constants.push_str(&format!("pub(crate) const {rust_name}: i32 = {};\n", value));
    }
    std::fs::write("src/abi_constants.rs", rust_constants).expect("write src/abi_constants.rs");

    let callback_name = callback
        .get("name")
        .unwrap_or_else(|| panic!("missing callback `name` in abi.toml"))
        .trim_matches('"');
    // `name` and `module` together determine the mangled symbol this crate
    // links against, and the build drivers derive the same tail from the same
    // fields (RFC 0004 §3.5). Nothing is hardcoded against them here: pinning
    // the name would defeat the point of abi.toml being the single source.
    let callback_module = callback
        .get("module")
        .unwrap_or_else(|| panic!("missing callback `module` in abi.toml"))
        .trim_matches('"');
    let params = callback
        .get("params")
        .unwrap_or_else(|| panic!("missing callback `params` in abi.toml"))
        .trim_matches(['[', ']'])
        .split(',')
        .map(|param| param.trim().trim_matches('"'))
        .collect::<Vec<_>>();
    if params != ["i32", "i32", "i32", "i32", "i32"] {
        panic!("abi.toml callback must take five i32 parameters");
    }
    let return_type = callback
        .get("return")
        .unwrap_or_else(|| panic!("missing callback `return` in abi.toml"))
        .trim_matches('"');
    if return_type != "i32" {
        panic!("abi.toml callback must return i32");
    }

    // --- Rust -> MoonBit callback symbol ---
    // The symbol is computed from abi.toml, and `mb_symbol.txt` overrides it
    // when present.
    //
    // The computed value is what makes this crate buildable on its own: since
    // RFC 0004 the callback is a library-owned entry point whose module path
    // and name are fixed and declared, so the mangling is deterministic. That
    // matters for publishing — `mb_symbol.txt` is gitignored and therefore not
    // part of `cargo package`, so a consumer building this crate from a
    // registry has nothing to read.
    //
    // `mb_symbol.txt` still wins where it exists, because `build.sh` writes the
    // *real* symbol extracted from MoonBit's compiled output: that tracks a
    // toolchain mangling change the computation would miss. A disagreement
    // between the two is exactly that situation, so it is reported rather than
    // silently accepted.
    println!("cargo:rerun-if-changed=mb_symbol.txt");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_TEST_DISPATCH_STUB");
    println!("cargo:rerun-if-env-changed=GPUI_SYS_ALLOW_TEST_DISPATCH_STUB");
    let test_stub_enabled = std::env::var_os("CARGO_FEATURE_TEST_DISPATCH_STUB").is_some();
    let extern_code = if test_stub_enabled {
        if std::env::var("GPUI_SYS_ALLOW_TEST_DISPATCH_STUB").as_deref() != Ok("1") {
            panic!(
                "feature `test-dispatch-stub` is test-only; set \
                 GPUI_SYS_ALLOW_TEST_DISPATCH_STUB=1 explicitly when running gpui-sys tests"
            );
        }
        // Route through the test dispatch recorder (lib.rs) when one is
        // installed, so the async-injection tests can observe dispatches and
        // drive the `changed` return value; otherwise behave as a plain no-op.
        "unsafe fn mb_dispatch(_version: i32, kind: i32, view: i32, data_a: i32, data_b: i32) -> i32 {\n    crate::dispatch_recorder::record(kind, view, data_a, data_b)\n}\n"
            .to_string()
    } else {
        let computed = mangled_callback_symbol(callback_module, callback_name);
        let extracted = std::fs::read_to_string("mb_symbol.txt")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let link_name = if extracted.is_empty() {
            computed
        } else {
            if extracted != computed {
                println!(
                    "cargo:warning=mb_symbol.txt ({extracted}) disagrees with the symbol \
                     computed from abi.toml ({computed}); using mb_symbol.txt. Either the \
                     MoonBit toolchain changed its mangling, or [callback] name/module in \
                     abi.toml is stale."
                );
            }
            extracted
        };
        // This declaration is generated only after validating the fixed-width
        // callback signature from abi.toml above. The five i32 slots carry the
        // versioned event envelope: (abi_version, event_kind, view, data_a, data_b).
        format!(
            "unsafe extern \"C\" {{\n    #[link_name = \"{link_name}\"]\n    fn mb_dispatch(version: i32, kind: i32, view: i32, data_a: i32, data_b: i32) -> i32;\n}}\n"
        )
    };
    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(Path::new(&out_dir).join("mb_extern.rs"), extern_code)
        .expect("write mb_extern.rs");

    // --- C header (cbindgen) ---
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let config = cbindgen::Config::from_file("cbindgen.toml").unwrap_or_default();
    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file("include/gpui_sys.h");
}
