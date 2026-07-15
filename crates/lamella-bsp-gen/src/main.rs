//! The bsp-gen CLI: `gen` writes a table's generated bindings under the bsp root; `check`
//! verifies the checked-in emission is regeneration-fresh (the CI/no-drift gate).

use std::path::Path;
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!(
        "usage: lamella-bsp-gen gen   <table.toml> <bsp-root>   write the generated bindings\n\
         \x20      lamella-bsp-gen check <table.toml> <bsp-root>   fail if the checked-in file is stale"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mode, table_path, bsp_root] = args.as_slice() else {
        return usage();
    };
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
