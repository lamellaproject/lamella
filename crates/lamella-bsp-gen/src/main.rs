//! The bsp-gen CLI: `gen` writes a v0 table's generated bindings under the bsp root; `check`
//! verifies the checked-in emission is regeneration-fresh (the CI/no-drift gate);
//! `gen-family`/`check-family` do the same for a whole v2 family (its layout + instances
//! classes and every board that names it).

use std::path::Path;
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!(
        "usage: lamella-bsp-gen gen          <table.toml> <bsp-root>   write a v0 table's bindings\n\
         \x20      lamella-bsp-gen check        <table.toml> <bsp-root>   fail if the v0 emission is stale\n\
         \x20      lamella-bsp-gen gen-family   <repo-root> <family>      write a v2 family's emissions\n\
         \x20      lamella-bsp-gen check-family <repo-root> <family>      fail if any v2 emission is stale\n\
         \x20      lamella-bsp-gen gen-parts    <repo-root> <family>      write a part family's emissions\n\
         \x20      lamella-bsp-gen check-parts  <repo-root> <family>      fail if any part emission is stale"
    );
    ExitCode::from(2)
}

fn run_family(mode: &str, repo_root: &str, family: &str) -> ExitCode {
    let root = Path::new(repo_root);
    let parts = mode.ends_with("-parts");
    let generated = if parts {
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
    let writer = if parts { "gen-parts" } else { "gen-family" };
    let mut stale = false;
    for file in &generated {
        let out_path = root.join(&file.path);
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mode, table_path, bsp_root] = args.as_slice() else {
        return usage();
    };
    if matches!(mode.as_str(), "gen-family" | "check-family" | "gen-parts" | "check-parts") {
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
