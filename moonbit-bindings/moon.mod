name = "nakake/gpui-bindings"

version = "0.1.0"

description = "Native GPUI bindings for MoonBit: GPU-accelerated desktop UI via a Rust staticlib bridge."

readme = "README.mbt.md"

repository = "https://github.com/nakake/gpui-moonbit"

license = "Apache-2.0"

keywords = [ "gpui", "moonbit", "gui", "native", "ui" ]

preferred_target = "native"

// Prebuild script (issue #93, G2): builds the Rust staticlib and propagates
// link flags to dependents. Runs on `moon build` / `moon test` and when this
// module is consumed as a path/git dependency. See build.py for details.
// WARNING: --moonbit-unstable-prebuild is extremely experimental; the API may
// change at any time. Only use with trusted dependencies.

options(
  "--moonbit-unstable-prebuild": "build.py",
)
