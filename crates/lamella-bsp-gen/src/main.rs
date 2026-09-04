//! The bsp-gen CLI: `gen` writes a v0 table's generated bindings under the bsp root; `check`
//! verifies the checked-in emission is regeneration-fresh (the CI/no-drift gate);
//! `gen-family`/`check-family` do the same for a whole v2 family (its layout + instances
//! classes and every board that names it); `gen-parts`/`check-parts` do the same for a part
//! family under `parts/` (each part's flattened table, in every language).

use std::path::Path;
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!(
        "usage: lamella-bsp-gen gen          <table.toml> <bsp-root>   write a v0 table's bindings\n\
         \x20      lamella-bsp-gen check        <table.toml> <bsp-root>   fail if the v0 emission is stale\n\
         \x20      lamella-bsp-gen gen-family   <repo-root> <family>      write a v2 family's emissions\n\
         \x20      lamella-bsp-gen check-family <repo-root> <family>      fail if any v2 emission is stale\n\
         \x20      lamella-bsp-gen gen-parts    <repo-root> <family>      write a part family's emissions\n\
         \x20      lamella-bsp-gen check-parts  <repo-root> <family>      fail if any part emission is stale\n\
         \x20      lamella-bsp-gen gen-ext      <repo-root> <extension>   write an extension board's emissions\n\
         \x20      lamella-bsp-gen check-ext    <repo-root> <extension>   fail if an extension emission is stale"
    );
    ExitCode::from(2)
}

fn run_family(mode: &str, repo_root: &str, family: &str) -> ExitCode {
    let root = Path::new(repo_root);
    let parts = mode.ends_with("-parts");
    let extension = mode.ends_with("-ext");
    let generated = if extension {
        lamella_bsp_gen::strata::generate_extension(root, family)
    } else if parts {
        lamella_bsp_gen::strata::generate_parts(root, family)
    } else {
        lamella_bsp_gen::strata::generate_family(root, family)
    };
    let generated = match generated {
        Ok(generated) => generated,
        Err(error) => {
            eprintln!("{family}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let writer = if extension {
        "gen-ext"
    } else if parts {
        "gen-parts"
    } else {
        "gen-family"
    };
    let mut stale = false;
    for file in &generated {
        let out_path = root.join(&file.path);
        if let Err(message) = refuse_case_fork(&out_path) {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
        if mode.starts_with("check") {
            match std::fs::read_to_string(&out_path) {
                Ok(existing) if existing.replace("\r\n", "\n") == file.contents => {
                    println!("{}: fresh", file.path);
                }
                Ok(_) => {
                    eprintln!("{}: STALE -- regenerate with `{writer}`", file.path);
                    stale = true;
                }
                Err(error) => {
                    eprintln!("{}: {error} -- generate with `{writer}`", file.path);
                    stale = true;
                }
            }
        } else {
            if let Some(parent) = out_path.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    eprintln!("{}: {error}", parent.display());
                    return ExitCode::FAILURE;
                }
            }
            if let Err(error) = std::fs::write(&out_path, &file.contents) {
                eprintln!("{}: {error}", out_path.display());
                return ExitCode::FAILURE;
            }
            println!("{}", file.path);
        }
    }
    if stale { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// Refuses to touch a target whose directory already holds a file differing from it only by CASE.
///
/// A GENERATOR THAT FORKS IS WORSE THAN ONE THAT FAILS, and this defect arrived reported from
/// outside rather than from any gate here. A board named with an internal word boundary can be
/// spelled two ways -- `MicrobitV1Bindings` against `MicroBitV1Bindings` -- and what happens next
/// depends on the filesystem, with three different bad endings:
///
///   case-sensitive     two files. The build keeps compiling the committed one, which is now
///                      stale and which no regeneration will ever touch again. It compiles
///                      cleanly, because a stale binding is still valid source.
///   case-insensitive   ONE file, whose CONTENT is replaced and whose NAME keeps the old
///                      spelling. Nothing looks new, so the fork hides as an ordinary edit.
///   mirrored between   whichever content the copy happens to walk last, matching neither the
///                      generator's output nor what the repository holds.
///
/// `read_dir` reports the name as it is STORED, so one check covers all three: a mismatch that
/// ignores case is the fork, whichever filesystem is underneath.
fn refuse_case_fork(out_path: &std::path::Path) -> Result<(), String> {
    let (Some(parent), Some(name)) = (out_path.parent(), out_path.file_name()) else {
        return Ok(());
    };
    let intended = name.to_string_lossy().to_string();
    let Ok(entries) = std::fs::read_dir(parent) else { return Ok(()) };
    for entry in entries.filter_map(Result::ok) {
        let present = entry.file_name().to_string_lossy().to_string();
        if present != intended && present.eq_ignore_ascii_case(&intended) {
            return Err(format!(
                "{}: a file named '{present}' is already here and differs from the intended \
                 '{intended}' only by case. Writing would FORK the artifact rather than update it \
                 -- rename the existing file to the intended spelling first. The generated name is \
                 the one that matches the type it declares.",
                parent.display()
            ));
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mode, table_path, bsp_root] = args.as_slice() else {
        return usage();
    };
    if matches!(
        mode.as_str(),
        "gen-family" | "check-family" | "gen-parts" | "check-parts" | "gen-ext" | "check-ext"
    ) {
        return run_family(mode, table_path, bsp_root);
    }
    if mode != "gen" && mode != "check" {
        return usage();
    }

    let text = match std::fs::read_to_string(table_path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("{table_path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let table = match lamella_bsp_gen::parse(&text) {
        Ok(table) => table,
        Err(error) => {
            eprintln!("{table_path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let source = table_path.replace('\\', "/");
    let source = source.split_once("docs/").map_or(source.clone(), |(_, tail)| format!("docs/{tail}"));
    let emitted = match lamella_bsp_gen::emit_csharp(&table, &source) {
        Ok(emitted) => emitted,
        Err(error) => {
            eprintln!("{table_path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let out_path = Path::new(bsp_root)
        .join(lamella_bsp_gen::csharp_path(&table).trim_start_matches("bsp/"));
    if mode == "check" {
        match std::fs::read_to_string(&out_path) {
            Ok(existing) if existing == emitted => {
                println!("{}: fresh", out_path.display());
                ExitCode::SUCCESS
            }
            Ok(_) => {
                eprintln!("{}: STALE -- regenerate with `gen`", out_path.display());
                ExitCode::FAILURE
            }
            Err(error) => {
                eprintln!("{}: {error} -- generate with `gen`", out_path.display());
                ExitCode::FAILURE
            }
        }
    } else {
        if let Some(parent) = out_path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                eprintln!("{}: {error}", parent.display());
                return ExitCode::FAILURE;
            }
        }
        if let Err(error) = std::fs::write(&out_path, &emitted) {
            eprintln!("{}: {error}", out_path.display());
            return ExitCode::FAILURE;
        }
        println!("{}", out_path.display());
        ExitCode::SUCCESS
    }
}
