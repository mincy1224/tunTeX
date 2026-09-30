//! `tuntex.exe` -- a drop-in stand-in for a local LaTeX compiler.
//!
//! Copy this executable to `latexmk.exe`, `xelatex.exe`, and friends to have
//! each name select its own backend automatically.  All configuration comes
//! from the environment, so the LaTeX command line passes through untouched.

fn main() {
    std::process::exit(tuntex_client::run());
}
