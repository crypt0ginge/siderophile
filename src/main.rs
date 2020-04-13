#![forbid(unsafe_code)]

#[macro_use]
extern crate log;
mod matching;
mod trawl_source;
use cargo::core::compiler::CompileMode;
use cargo::core::shell::Shell;
use cargo::ops::CompileOptions;
use cargo::{core::resolver::Method, CliError};
use glob::glob;
use std::path::PathBuf;
use std::process::Command;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
pub struct Args {
    #[structopt(long = "build_plan")]
    /// Output a build plan to stdout instead of actually compiling
    build_plan: bool,

    #[structopt(long = "features", value_name = "FEATURES")]
    /// Space-separated list of features to activate
    features: Option<String>,

    #[structopt(long = "all-features")]
    /// Activate all available features
    all_features: bool,

    #[structopt(long = "no-default-features")]
    /// Do not activate the `default` feature
    no_default_features: bool,

    #[structopt(
        long = "manifest-path",
        value_name = "MANIFEST_PATH",
        parse(from_os_str)
    )]
    /// Path to Cargo.toml
    manifest_path: Option<PathBuf>,

    #[structopt(long = "jobs", short = "j")]
    /// Number of parallel jobs, defaults to # of CPUs
    jobs: Option<u32>,

    #[structopt(long = "verbose", short = "v", parse(from_occurrences))]
    /// Use verbose cargo output (-vv very verbose)
    verbose: u32,

    #[structopt(long = "quiet", short = "q")]
    /// Omit cargo output to stdout
    quiet: bool,

    #[structopt(long = "color", value_name = "WHEN")]
    /// Cargo output coloring: auto, always, never
    color: Option<String>,

    #[structopt(long = "frozen")]
    /// Require Cargo.lock and cache are up to date
    frozen: bool,

    #[structopt(long = "locked")]
    /// Require Cargo.lock is up to date
    locked: bool,

    #[structopt(short = "Z", value_name = "FLAG")]
    /// Unstable (nightly-only) flags to Cargo
    unstable_flags: Vec<String>,

    #[structopt(long = "include-tests")]
    /// Count unsafe usage in tests.
    include_tests: bool,

    #[structopt(long = "crate-name", value_name = "NAME")]
    /// crate name
    crate_name: String,
}

/// Based on code from cargo-bloat. It seems weird that CompileOptions can be
/// constructed without providing all standard cargo options, TODO: Open an issue
/// in cargo?
fn build_compile_options<'a>(args: &'a Args, config: &'a cargo::Config) -> CompileOptions<'a> {
    let features = Method::split_features(&args.features.clone().into_iter().collect::<Vec<_>>())
        .into_iter()
        .map(|s| s.to_string());
    let mut opt = CompileOptions::new(&config, CompileMode::Check { test: false }).unwrap();
    opt.features = features.collect::<_>();
    opt.all_features = args.all_features;
    opt.no_default_features = args.no_default_features;

    // BuildConfig, see https://docs.rs/cargo/0.31.0/cargo/core/compiler/struct.BuildConfig.html
    if let Some(jobs) = args.jobs {
        opt.build_config.jobs = jobs;
    }

    opt.build_config.build_plan = args.build_plan;
    opt
}

fn find_unsafety(args: &Args) -> Result<Vec<String>, CliError> {
    let mut config = match cargo::Config::default() {
        Ok(cfg) => cfg,
        Err(e) => {
            let mut shell = Shell::new();
            cargo::exit_with_error(e.into(), &mut shell)
        }
    };
    let target_dir = None;

    config.configure(
        args.verbose,
        Some(args.quiet),
        &args.color,
        args.frozen,
        args.locked,
        &target_dir,
        &args.unstable_flags,
    )?;

    let ws = trawl_source::workspace(&config, args.manifest_path.clone())?;
    let (packages, _) = cargo::ops::resolve_ws(&ws)?;

    info!("rustc config == {:?}", config.rustc(Some(&ws)));

    let copt = build_compile_options(args, &config);
    let rs_files_used_in_compilation = trawl_source::resolve_rs_file_deps(&copt, &ws).unwrap();

    let allow_partial_results = true;

    let (rs_files_scanned, unsafe_things) = trawl_source::find_unsafe_in_packages(
        &packages,
        rs_files_used_in_compilation,
        allow_partial_results,
        args.include_tests,
    );

    rs_files_scanned
        .iter()
        .filter(|(_k, v)| **v == 0)
        .for_each(|(k, _v)| {
            // TODO: Ivestigate if this is related to code generated by build
            // scripts and/or macros. Some of the warnings of this kind is
            // printed for files somewhere under the "target" directory.
            // TODO: Find out if we can lookup PackageId associated with each
            // `.rs` file used by the build, including the file paths extracted
            // from `.d` dep files.
            warn!("Dependency file was never scanned: {}", k.display())
        });

    Ok(unsafe_things)
}

fn generate_llvm_bytecode() {
    let status = Command::new("cargo")
        .args(&["clean"])
        .status()
        .expect("failed to clean workspace before generating bytecode");
    if !status.success() {
        panic!("could not clean workspace before generating bytecode");
    }
    let status = Command::new("cargo")
        .args(&["rustc", "--", "--emit=llvm-bc"])
        .env(
            "RUSTFLAGS",
            "-C lto=no -C opt-level=0 -C debuginfo=2 --emit=llvm-bc",
        )
        .env("CARGO_INCREMENTAL", "0")
        .status()
        .expect("could not call cargo to generate llvm bytecode");
    if !status.success() {
        panic!("could not generate llvm bytecode");
    }
}

// If we're in a crate in a workspace, check the directory above for the compiler output
fn generate_callgraph(cratename: &String) {
    let bytecode = if let Some(Ok(bytecode_dir)) =
        glob(&*format!("./target/debug/deps/{}-*.bc", cratename))
            .expect("Failed to read glob pattern")
            .next()
    {
        bytecode_dir
    } else if let Some(Ok(bytecode_parentdir)) =
        glob(&*format!("./target/debug/deps/{}-*.bc", cratename))
            .expect("Failed to read glob pattern")
            .next()
    {
        bytecode_parentdir
    } else {
        panic!("can't find bytecode file.")
    };

    // output is useless
    let _output = Command::new("opt")
        .args(&["-dot-callgraph", bytecode.to_str().unwrap()])
        .output()
        .expect("could not generate callgraph");
}

fn main() {
    // TODO: add proper error handling and logging
    // TODO: add log statements documenting what's going on
    // TODO: clean everything up, really. what a mess.
    // TODO: cargo test :)
    env_logger::init();
    // All auxiliary files go here
    generate_llvm_bytecode();
    let args = Args::from_args();
    generate_callgraph(&args.crate_name);
    let unsafe_deps = find_unsafety(&args).unwrap();
    matching::callgraph_matching(
        &PathBuf::from("./callgraph.dot"),
        unsafe_deps,
        args.crate_name,
    )
    .unwrap();
}
