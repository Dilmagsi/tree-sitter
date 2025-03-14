use anyhow::{anyhow, Context, Error, Result};
use clap::{App, AppSettings, Arg, SubCommand};
use glob::glob;
use regex::Regex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::{env, fs, u64};
use tree_sitter::{ffi, Parser, Point};
use tree_sitter_cli::test::TestOptions;
use tree_sitter_cli::{
    generate, highlight, logger,
    parse::{self, ParseFileOptions, ParseOutput},
    playground, query, tags, test, test_highlight, test_tags, util, wasm,
};
use tree_sitter_config::Config;
use tree_sitter_highlight::Highlighter;
use tree_sitter_loader as loader;
use tree_sitter_tags::TagsContext;

const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_SHA: Option<&'static str> = option_env!("BUILD_SHA");
const DEFAULT_GENERATE_ABI_VERSION: usize = 14;

fn main() {
    let result = run();
    if let Err(err) = &result {
        // Ignore BrokenPipe errors
        if let Some(error) = err.downcast_ref::<std::io::Error>() {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return;
            }
        }
        if !err.to_string().is_empty() {
            eprintln!("{err:?}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let version = BUILD_SHA.map_or_else(
        || BUILD_VERSION.to_string(),
        |build_sha| format!("{BUILD_VERSION} ({build_sha})"),
    );

    let debug_arg = Arg::with_name("debug")
        .help("Show parsing debug log")
        .long("debug")
        .short("d");

    let debug_graph_arg = Arg::with_name("debug-graph")
        .help("Produce the log.html file with debug graphs")
        .long("debug-graph")
        .short("D");

    let debug_build_arg = Arg::with_name("debug-build")
        .help("Compile a parser in debug mode")
        .long("debug-build")
        .short("0");

    let paths_file_arg = Arg::with_name("paths-file")
        .help("The path to a file with paths to source file(s)")
        .long("paths")
        .takes_value(true);

    let paths_arg = Arg::with_name("paths")
        .help("The source file(s) to use")
        .multiple(true);

    let scope_arg = Arg::with_name("scope")
        .help("Select a language by the scope instead of a file extension")
        .long("scope")
        .takes_value(true);

    let time_arg = Arg::with_name("time")
        .help("Measure execution time")
        .long("time")
        .short("t");

    let quiet_arg = Arg::with_name("quiet")
        .help("Suppress main output")
        .long("quiet")
        .short("q");

    let wasm_arg = Arg::with_name("wasm")
        .long("wasm")
        .help("compile parsers to wasm instead of native dynamic libraries");
    let apply_all_captures_arg = Arg::with_name("apply-all-captures")
        .help("Apply all captures to highlights")
        .long("apply-all-captures");

    let matches = App::new("tree-sitter")
        .author("Max Brunsfeld <maxbrunsfeld@gmail.com>")
        .about("Generates and tests parsers")
        .version(version.as_str())
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .global_setting(AppSettings::ColoredHelp)
        .global_setting(AppSettings::DeriveDisplayOrder)
        .global_setting(AppSettings::DisableHelpSubcommand)
        .subcommand(SubCommand::with_name("init-config").about("Generate a default config file"))
        .subcommand(
            SubCommand::with_name("generate")
                .alias("gen")
                .alias("g")
                .about("Generate a parser")
                .arg(Arg::with_name("grammar-path").index(1))
                .arg(Arg::with_name("log").long("log"))
                .arg(
                    Arg::with_name("abi-version")
                        .long("abi")
                        .value_name("version")
                        .help(&format!(
                            concat!(
                                "Select the language ABI version to generate (default {}).\n",
                                "Use --abi=latest to generate the newest supported version ({}).",
                            ),
                            DEFAULT_GENERATE_ABI_VERSION,
                            tree_sitter::LANGUAGE_VERSION,
                        )),
                )
                .arg(Arg::with_name("no-bindings").long("no-bindings"))
                .arg(
                    Arg::with_name("build")
                        .long("build")
                        .short("b")
                        .help("Compile all defined languages in the current dir"),
                )
                .arg(&debug_build_arg)
                .arg(
                    Arg::with_name("libdir")
                        .long("libdir")
                        .takes_value(true)
                        .value_name("path"),
                )
                .arg(
                    Arg::with_name("report-states-for-rule")
                        .long("report-states-for-rule")
                        .value_name("rule-name")
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("js-runtime")
                        .long("js-runtime")
                        .takes_value(true)
                        .value_name("executable")
                        .env("TREE_SITTER_JS_RUNTIME")
                        .help("Use a JavaScript runtime other than node"),
                ),
        )
        .subcommand(
            SubCommand::with_name("parse")
                .alias("p")
                .about("Parse files")
                .arg(&paths_file_arg)
                .arg(&paths_arg)
                .arg(&scope_arg)
                .arg(&debug_arg)
                .arg(&debug_build_arg)
                .arg(&debug_graph_arg)
                .arg(&wasm_arg)
                .arg(Arg::with_name("output-dot").long("dot"))
                .arg(Arg::with_name("output-xml").long("xml").short("x"))
                .arg(
                    Arg::with_name("stat")
                        .help("Show parsing statistic")
                        .long("stat")
                        .short("s"),
                )
                .arg(
                    Arg::with_name("timeout")
                        .help("Interrupt the parsing process by timeout (µs)")
                        .long("timeout")
                        .takes_value(true),
                )
                .arg(&time_arg)
                .arg(&quiet_arg)
                .arg(
                    Arg::with_name("edits")
                        .help("Apply edits in the format: \"row,col del_count insert_text\"")
                        .long("edit")
                        .short("edit")
                        .takes_value(true)
                        .multiple(true)
                        .number_of_values(1),
                )
                .arg(
                    Arg::with_name("encoding")
                        .help("The encoding of the input files")
                        .long("encoding")
                        .takes_value(true),
                ),
        )
        .subcommand(
            SubCommand::with_name("query")
                .alias("q")
                .about("Search files using a syntax tree query")
                .arg(
                    Arg::with_name("query-path")
                        .help("Path to a file with queries")
                        .index(1)
                        .required(true),
                )
                .arg(&time_arg)
                .arg(&quiet_arg)
                .arg(&paths_file_arg)
                .arg(&paths_arg.clone().index(2))
                .arg(
                    Arg::with_name("byte-range")
                        .help("The range of byte offsets in which the query will be executed")
                        .long("byte-range")
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("row-range")
                        .help("The range of rows in which the query will be executed")
                        .long("row-range")
                        .takes_value(true),
                )
                .arg(&scope_arg)
                .arg(Arg::with_name("captures").long("captures").short("c"))
                .arg(Arg::with_name("test").long("test")),
        )
        .subcommand(
            SubCommand::with_name("tags")
                .about("Generate a list of tags")
                .arg(&scope_arg)
                .arg(&time_arg)
                .arg(&quiet_arg)
                .arg(&paths_file_arg)
                .arg(&paths_arg),
        )
        .subcommand(
            SubCommand::with_name("test")
                .alias("t")
                .about("Run a parser's tests")
                .arg(
                    Arg::with_name("filter")
                        .long("filter")
                        .short("f")
                        .takes_value(true)
                        .help("[DEPRECATED in favor of include]\nOnly run corpus test cases whose name includes the given string"),
                )
                .arg(
                    Arg::with_name("include")
                        .long("include")
                        .short("i")
                        .takes_value(true)
                        .help("Only run corpus test cases whose name matches the given regex"),
                )
                .arg(
                    Arg::with_name("exclude")
                        .long("exclude")
                        .short("e")
                        .takes_value(true)
                        .help(
                            "Only run corpus test cases whose name does not match the given regex",
                        ),
                )
                .arg(
                    Arg::with_name("update")
                        .long("update")
                        .short("u")
                        .help("Update all syntax trees in corpus files with current parser output"),
                )
                .arg(&debug_arg)
                .arg(&debug_build_arg)
                .arg(&debug_graph_arg)
                .arg(&wasm_arg)
                .arg(&apply_all_captures_arg),
        )
        .subcommand(
            SubCommand::with_name("highlight")
                .about("Highlight a file")
                .arg(
                    Arg::with_name("html")
                        .help("Generate highlighting as an HTML document")
                        .long("html")
                        .short("H"),
                )
                .arg(
                    Arg::with_name("check")
                        .help("Check that highlighting captures conform strictly to standards")
                        .long("check"),
                )
                .arg(
                    Arg::with_name("captures-path")
                        .help("Path to a file with captures")
                        .long("captures-path")
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("query-paths")
                        .help("Paths to files with queries")
                        .long("query-paths")
                        .takes_value(true)
                        .multiple(true)
                        .number_of_values(1),
                )
                .arg(&scope_arg)
                .arg(&time_arg)
                .arg(&quiet_arg)
                .arg(&paths_file_arg)
                .arg(&paths_arg)
                .arg(&apply_all_captures_arg),
        )
        .subcommand(
            SubCommand::with_name("build-wasm")
                .alias("bw")
                .about("Compile a parser to WASM")
                .arg(
                    Arg::with_name("docker").long("docker").help(
                        "Run emscripten via docker or podman even if it is installed locally",
                    ),
                )
                .arg(Arg::with_name("path").index(1).multiple(true)),
        )
        .subcommand(
            SubCommand::with_name("playground")
                .alias("play")
                .alias("pg")
                .alias("web-ui")
                .about("Start local playground for a parser in the browser")
                .arg(
                    Arg::with_name("quiet")
                        .long("quiet")
                        .short("q")
                        .help("Don't open in default browser"),
                ),
        )
        .subcommand(
            SubCommand::with_name("dump-languages")
                .about("Print info about all known language parsers"),
        )
        .get_matches();

    let current_dir = env::current_dir().unwrap();
    let config = Config::load()?;
    let mut loader = loader::Loader::new()?;

    match matches.subcommand() {
        ("init-config", Some(_)) => {
            if let Ok(Some(config_path)) = Config::find_config_file() {
                return Err(anyhow!(
                    "Remove your existing config file first: {}",
                    config_path.to_string_lossy()
                ));
            }
            let mut config = Config::initial()?;
            config.add(tree_sitter_loader::Config::initial())?;
            config.add(tree_sitter_cli::highlight::ThemeConfig::default())?;
            config.save()?;
            println!(
                "Saved initial configuration to {}",
                config.location.display()
            );
        }

        ("generate", Some(matches)) => {
            let grammar_path = matches.value_of("grammar-path");
            let debug_build = matches.is_present("debug-build");
            let build = matches.is_present("build");
            let libdir = matches.value_of("libdir");
            let js_runtime = matches.value_of("js-runtime");
            let report_symbol_name = matches.value_of("report-states-for-rule").or_else(|| {
                if matches.is_present("report-states") {
                    Some("")
                } else {
                    None
                }
            });
            if matches.is_present("log") {
                logger::init();
            }
            let abi_version = matches.value_of("abi-version").map_or(
                Ok::<_, Error>(DEFAULT_GENERATE_ABI_VERSION),
                |version| {
                    Ok(if version == "latest" {
                        tree_sitter::LANGUAGE_VERSION
                    } else {
                        version
                            .parse()
                            .with_context(|| "invalid abi version flag")?
                    })
                },
            )?;
            let generate_bindings = !matches.is_present("no-bindings");
            generate::generate_parser_in_directory(
                &current_dir,
                grammar_path,
                abi_version,
                generate_bindings,
                report_symbol_name,
                js_runtime,
            )?;
            if build {
                if let Some(path) = libdir {
                    loader = loader::Loader::with_parser_lib_path(PathBuf::from(path));
                }
                loader.use_debug_build(debug_build);
                loader.languages_at_path(&current_dir)?;
            }
        }

        ("test", Some(matches)) => {
            let debug = matches.is_present("debug");
            let debug_graph = matches.is_present("debug-graph");
            let debug_build = matches.is_present("debug-build");
            let update = matches.is_present("update");
            let filter = matches.value_of("filter");
            let include: Option<Regex> =
                matches.value_of("include").and_then(|s| Regex::new(s).ok());
            let exclude: Option<Regex> =
                matches.value_of("exclude").and_then(|s| Regex::new(s).ok());
            let apply_all_captures = matches.is_present("apply-all-captures");

            if debug {
                // For augmenting debug logging in external scanners
                env::set_var("TREE_SITTER_DEBUG", "1");
            }

            loader.use_debug_build(debug_build);

            let mut parser = Parser::new();

            #[cfg(feature = "wasm")]
            if matches.is_present("wasm") {
                let engine = tree_sitter::wasmtime::Engine::default();
                parser
                    .set_wasm_store(tree_sitter::WasmStore::new(engine.clone()).unwrap())
                    .unwrap();
                loader.use_wasm(engine);
            }

            let languages = loader.languages_at_path(&current_dir)?;
            let language = languages
                .first()
                .ok_or_else(|| anyhow!("No language found"))?;
            parser.set_language(language)?;

            let test_dir = current_dir.join("test");

            // Run the corpus tests. Look for them at two paths: `test/corpus` and `corpus`.
            let mut test_corpus_dir = test_dir.join("corpus");
            if !test_corpus_dir.is_dir() {
                test_corpus_dir = current_dir.join("corpus");
            }
            if test_corpus_dir.is_dir() {
                let mut opts = TestOptions {
                    path: test_corpus_dir,
                    debug,
                    debug_graph,
                    filter,
                    include,
                    exclude,
                    update,
                };

                test::run_tests_at_path(&mut parser, &mut opts)?;
            }

            // Check that all of the queries are valid.
            test::check_queries_at_path(language, &current_dir.join("queries"))?;

            // Run the syntax highlighting tests.
            let test_highlight_dir = test_dir.join("highlight");
            if test_highlight_dir.is_dir() {
                let mut highlighter = Highlighter::new();
                highlighter.parser = parser;
                test_highlight::test_highlights(
                    &loader,
                    &mut highlighter,
                    &test_highlight_dir,
                    apply_all_captures,
                )?;
                parser = highlighter.parser;
            }

            let test_tag_dir = test_dir.join("tags");
            if test_tag_dir.is_dir() {
                let mut tags_context = TagsContext::new();
                tags_context.parser = parser;
                test_tags::test_tags(&loader, &mut tags_context, &test_tag_dir)?;
            }
        }

        ("parse", Some(matches)) => {
            let debug = matches.is_present("debug");
            let debug_graph = matches.is_present("debug-graph");
            let debug_build = matches.is_present("debug-build");

            let output = if matches.is_present("output-dot") {
                ParseOutput::Dot
            } else if matches.is_present("output-xml") {
                ParseOutput::Xml
            } else if matches.is_present("quiet") {
                ParseOutput::Quiet
            } else {
                ParseOutput::Normal
            };

            let encoding =
                matches
                    .values_of("encoding")
                    .map_or(Ok(None), |mut e| match e.next() {
                        Some("utf16") => Ok(Some(ffi::TSInputEncodingUTF16)),
                        Some("utf8") => Ok(Some(ffi::TSInputEncodingUTF8)),
                        Some(_) => Err(anyhow!("Invalid encoding. Expected one of: utf8, utf16")),
                        None => Ok(None),
                    })?;

            let time = matches.is_present("time");
            let edits = matches
                .values_of("edits")
                .map_or(Vec::new(), std::iter::Iterator::collect);
            let cancellation_flag = util::cancel_on_signal();
            let mut parser = Parser::new();

            if debug {
                // For augmenting debug logging in external scanners
                env::set_var("TREE_SITTER_DEBUG", "1");
            }

            loader.use_debug_build(debug_build);

            #[cfg(feature = "wasm")]
            if matches.is_present("wasm") {
                let engine = tree_sitter::wasmtime::Engine::default();
                parser
                    .set_wasm_store(tree_sitter::WasmStore::new(engine.clone()).unwrap())
                    .unwrap();
                loader.use_wasm(engine);
            }

            let timeout = matches
                .value_of("timeout")
                .map_or(0, |t| t.parse::<u64>().unwrap());

            let paths = collect_paths(matches.value_of("paths-file"), matches.values_of("paths"))?;

            let max_path_length = paths.iter().map(|p| p.chars().count()).max().unwrap_or(0);
            let mut has_error = false;
            let loader_config = config.get()?;
            loader.find_all_languages(&loader_config)?;

            let should_track_stats = matches.is_present("stat");
            let mut stats = parse::Stats::default();

            for path in paths {
                let path = Path::new(&path);

                let language =
                    loader.select_language(path, &current_dir, matches.value_of("scope"))?;
                parser
                    .set_language(&language)
                    .context("incompatible language")?;

                let opts = ParseFileOptions {
                    language: language.clone(),
                    path,
                    edits: &edits,
                    max_path_length,
                    output,
                    print_time: time,
                    timeout,
                    debug,
                    debug_graph,
                    cancellation_flag: Some(&cancellation_flag),
                    encoding,
                };

                let parse_result = parse::parse_file_at_path(&mut parser, &opts)?;

                if should_track_stats {
                    stats.total_parses += 1;
                    if parse_result.successful {
                        stats.successful_parses += 1;
                    }
                    if let Some(duration) = parse_result.duration {
                        stats.total_bytes += parse_result.bytes;
                        stats.total_duration += duration;
                    }
                }

                has_error |= !parse_result.successful;
            }

            if should_track_stats {
                println!("\n{stats}");
            }

            if has_error {
                return Err(anyhow!(""));
            }
        }

        ("query", Some(matches)) => {
            let ordered_captures = matches.values_of("captures").is_some();
            let quiet = matches.values_of("quiet").is_some();
            let time = matches.values_of("time").is_some();
            let paths = collect_paths(matches.value_of("paths-file"), matches.values_of("paths"))?;
            let loader_config = config.get()?;
            loader.find_all_languages(&loader_config)?;
            let language = loader.select_language(
                Path::new(&paths[0]),
                &current_dir,
                matches.value_of("scope"),
            )?;
            let query_path = Path::new(matches.value_of("query-path").unwrap());
            let byte_range = matches.value_of("byte-range").and_then(|arg| {
                let mut parts = arg.split(':');
                let start = parts.next()?.parse().ok()?;
                let end = parts.next().unwrap().parse().ok()?;
                Some(start..end)
            });
            let point_range = matches.value_of("row-range").and_then(|arg| {
                let mut parts = arg.split(':');
                let start = parts.next()?.parse().ok()?;
                let end = parts.next().unwrap().parse().ok()?;
                Some(Point::new(start, 0)..Point::new(end, 0))
            });
            let should_test = matches.is_present("test");
            query::query_files_at_paths(
                &language,
                paths,
                query_path,
                ordered_captures,
                byte_range,
                point_range,
                should_test,
                quiet,
                time,
            )?;
        }

        ("tags", Some(matches)) => {
            let loader_config = config.get()?;
            loader.find_all_languages(&loader_config)?;
            let paths = collect_paths(matches.value_of("paths-file"), matches.values_of("paths"))?;
            tags::generate_tags(
                &loader,
                matches.value_of("scope"),
                &paths,
                matches.is_present("quiet"),
                matches.is_present("time"),
            )?;
        }

        ("highlight", Some(matches)) => {
            let theme_config: tree_sitter_cli::highlight::ThemeConfig = config.get()?;
            loader.configure_highlights(&theme_config.theme.highlight_names);
            let loader_config = config.get()?;
            loader.find_all_languages(&loader_config)?;

            let time = matches.is_present("time");
            let quiet = matches.is_present("quiet");
            let html_mode = quiet || matches.is_present("html");
            let should_check = matches.is_present("check");
            let paths = collect_paths(matches.value_of("paths-file"), matches.values_of("paths"))?;
            let apply_all_captures = matches.is_present("apply-all-captures");

            if html_mode && !quiet {
                println!("{}", highlight::HTML_HEADER);
            }

            let cancellation_flag = util::cancel_on_signal();

            let mut language = None;
            if let Some(scope) = matches.value_of("scope") {
                language = loader.language_configuration_for_scope(scope)?;
                if language.is_none() {
                    return Err(anyhow!("Unknown scope '{scope}'"));
                }
            }

            let query_paths = matches.values_of("query-paths").map(|e| {
                e.collect::<Vec<_>>()
                    .into_iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
            });

            for path in paths {
                let path = Path::new(&path);
                let (language, language_config) = match language.clone() {
                    Some(v) => v,
                    None => {
                        if let Some(v) = loader.language_configuration_for_file_name(path)? {
                            v
                        } else {
                            eprintln!("No language found for path {path:?}");
                            continue;
                        }
                    }
                };

                if let Some(highlight_config) = language_config.highlight_config(
                    language,
                    apply_all_captures,
                    query_paths.as_deref(),
                )? {
                    if should_check {
                        let names = if let Some(path) = matches.value_of("captures-path") {
                            let path = Path::new(path);
                            let file = fs::read_to_string(path)?;
                            let capture_names = file
                                .lines()
                                .filter_map(|line| {
                                    if line.trim().is_empty() || line.trim().starts_with(';') {
                                        return None;
                                    }
                                    line.split(';').next().map(|s| s.trim().trim_matches('"'))
                                })
                                .collect::<HashSet<_>>();
                            highlight_config.nonconformant_capture_names(&capture_names)
                        } else {
                            highlight_config.nonconformant_capture_names(&HashSet::new())
                        };
                        if names.is_empty() {
                            eprintln!("All highlight captures conform to standards.");
                        } else {
                            eprintln!(
                                "Non-standard highlight {} detected:",
                                if names.len() > 1 {
                                    "captures"
                                } else {
                                    "capture"
                                }
                            );
                            for name in names {
                                eprintln!("* {name}");
                            }
                        }
                    }

                    let source = fs::read(path)?;
                    if html_mode {
                        highlight::html(
                            &loader,
                            &theme_config.theme,
                            &source,
                            highlight_config,
                            quiet,
                            time,
                            Some(&cancellation_flag),
                        )?;
                    } else {
                        highlight::ansi(
                            &loader,
                            &theme_config.theme,
                            &source,
                            highlight_config,
                            time,
                            Some(&cancellation_flag),
                        )?;
                    }
                } else {
                    eprintln!("No syntax highlighting config found for path {path:?}");
                }
            }

            if html_mode && !quiet {
                println!("{}", highlight::HTML_FOOTER);
            }
        }

        ("build-wasm", Some(matches)) => {
            let grammar_path = current_dir.join(matches.value_of("path").unwrap_or(""));
            wasm::compile_language_to_wasm(
                &loader,
                &grammar_path,
                &current_dir,
                matches.is_present("docker"),
            )?;
        }

        ("playground", Some(matches)) => {
            let open_in_browser = !matches.is_present("quiet");
            playground::serve(&current_dir, open_in_browser)?;
        }

        ("dump-languages", Some(_)) => {
            let loader_config = config.get()?;
            loader.find_all_languages(&loader_config)?;
            for (configuration, language_path) in loader.get_all_language_configurations() {
                println!(
                    concat!(
                        "scope: {}\n",
                        "parser: {:?}\n",
                        "highlights: {:?}\n",
                        "file_types: {:?}\n",
                        "content_regex: {:?}\n",
                        "injection_regex: {:?}\n",
                    ),
                    configuration.scope.as_ref().unwrap_or(&String::new()),
                    language_path,
                    configuration.highlights_filenames,
                    configuration.file_types,
                    configuration.content_regex,
                    configuration.injection_regex,
                );
            }
        }

        _ => unreachable!(),
    }

    Ok(())
}

fn collect_paths<'a>(
    paths_file: Option<&str>,
    paths: Option<impl Iterator<Item = &'a str>>,
) -> Result<Vec<String>> {
    if let Some(paths_file) = paths_file {
        return Ok(fs::read_to_string(paths_file)
            .with_context(|| format!("Failed to read paths file {paths_file}"))?
            .trim()
            .lines()
            .map(String::from)
            .collect::<Vec<_>>());
    }

    if let Some(paths) = paths {
        let mut result = Vec::new();

        let mut incorporate_path = |path: &str, positive| {
            if positive {
                result.push(path.to_string());
            } else if let Some(index) = result.iter().position(|p| p == path) {
                result.remove(index);
            }
        };

        for mut path in paths {
            let mut positive = true;
            if path.starts_with('!') {
                positive = false;
                path = path.trim_start_matches('!');
            }

            if Path::new(path).exists() {
                incorporate_path(path, positive);
            } else {
                let paths = glob(path).with_context(|| format!("Invalid glob pattern {path:?}"))?;
                for path in paths {
                    if let Some(path) = path?.to_str() {
                        incorporate_path(path, positive);
                    }
                }
            }
        }

        if result.is_empty() {
            return Err(anyhow!(
                "No files were found at or matched by the provided pathname/glob"
            ));
        }

        return Ok(result);
    }

    Err(anyhow!("Must provide one or more paths"))
}
