//! Arguments are fowarded directly to wasm-pack

use std::{
    convert::TryInto,
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Result};

const OUT_DIR: &str = "build";
const OUT_NAME: &str = "index";
const WORKER_SUBDIR: &str = "worker";

pub fn main() -> Result<()> {
    check_wasm_pack_installed()?;
    wasm_pack_build(env::args_os().skip(1))?;
    create_worker_dir()?;
    copy_generated_code_to_worker_dir()?;
    write_worker_shims_to_worker_dir()?;
    replace_generated_import_with_custom_impl()?;

    Ok(())
}

fn check_wasm_pack_installed() -> Result<()> {
    match Command::new("wasm-pack").output() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            println!("Installing wasm-pack...");
            let exit_status = Command::new("cargo")
                .args(&["install", "wasm-pack"])
                .spawn()?
                .wait()?;

            match exit_status.success() {
                true => Ok(()),
                false => Err(anyhow!(
                    "installation of wasm-pack exited with status {}",
                    exit_status
                )),
            }
        }
        _ => Ok(()),
    }
}

fn wasm_pack_build<I, S>(args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let exit_status = Command::new("wasm-pack")
        .arg("build")
        .arg("--no-typescript")
        .args(&["--out-dir", OUT_DIR])
        .args(&["--out-name", OUT_NAME])
        .args(args)
        .spawn()?
        .wait()?;

    match exit_status.success() {
        true => Ok(()),
        false => Err(anyhow!("wasm-pack exited with status {}", exit_status)),
    }
}

fn create_worker_dir() -> Result<()> {
    // create a directory for our worker to live in
    let worker_dir = PathBuf::from(OUT_DIR).join(WORKER_SUBDIR);

    // remove anything that already exists
    if worker_dir.is_dir() {
        fs::remove_dir_all(&worker_dir)?
    } else if worker_dir.is_file() {
        fs::remove_file(&worker_dir)?
    };

    // create an output dir
    fs::create_dir(worker_dir)?;

    Ok(())
}

fn copy_generated_code_to_worker_dir() -> Result<()> {
    let glue_src = PathBuf::from(OUT_DIR).join(format!("{}_bg.js", OUT_NAME));
    let glue_dest = PathBuf::from(OUT_DIR)
        .join(WORKER_SUBDIR)
        .join(format!("{}_bg.mjs", OUT_NAME));

    let wasm_src = PathBuf::from(OUT_DIR).join(format!("{}_bg.wasm", OUT_NAME));
    let wasm_dest = PathBuf::from(OUT_DIR)
        .join(WORKER_SUBDIR)
        .join(format!("{}_bg.wasm", OUT_NAME));

    for (src, dest) in [(glue_src, glue_dest), (wasm_src, wasm_dest)] {
        fs::rename(src, dest)?;
    }

    Ok(())
}

fn write_worker_shims_to_worker_dir() -> Result<()> {
    // _bg implies "bindgen" from wasm-bindgen tooling
    let bg_name = format!("{}_bg", OUT_NAME);

    // this shim just re-exports things in the pattern workers expects
    let shim_content = format!(
        r#"
import * as imports from "./{0}.mjs";

const fetch = imports.fetch;
const scheduled = imports.scheduled;

export * from "./{0}.mjs";
export default {{ fetch, scheduled }};

"#,
        bg_name
    );
    let shim_path = PathBuf::from(OUT_DIR).join(WORKER_SUBDIR).join("shim.mjs");

    // write our content out to files
    let mut file = File::create(shim_path)?;
    file.write_all(shim_content.as_bytes())?;

    Ok(())
}

fn replace_generated_import_with_custom_impl() -> Result<()> {
    let bg_name = format!("{}_bg", OUT_NAME);
    let bindgen_glue_path = PathBuf::from(OUT_DIR)
        .join(WORKER_SUBDIR)
        .join(format!("{}_bg.mjs", OUT_NAME));
    let old_bindgen_glue = read_file_to_string(&bindgen_glue_path)?;
    let old_import = format!("import * as wasm from './{}_bg.wasm'", OUT_NAME);
    let mut fixed_bindgen_glue = old_bindgen_glue.replace(&old_import, "let wasm;");

    // Previously we just used a namespace import as the wasm's import obj but this had a problem
    // where wasm-bindgen would re-export built-in functions as a const variable. This caused a
    // problem because the OUTNAME_bg.mjs imported the wasm from another file causing a circular
    // import so the const function items were never initialized.
    //
    // Because can't use import OUTNAME_bg.mjs module and use that module's exports as the import
    // object, we'll have to find all the exports and use that to construct the wasm's import obj.
    let names_for_import_obj = find_exports(&fixed_bindgen_glue);

    // this shim exports a webassembly instance with 512 pages
    // https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/WebAssembly/Memory/Memory#parameters
    let initialization_glue = format!(
        r#"
import _wasm from "./{0}.wasm";

const _wasm_memory = new WebAssembly.Memory({{initial: 512}});
let importsObject = {{
    env: {{ memory: _wasm_memory }},
    "./{0}.js": {{ {1} }}
}};

wasm = new WebAssembly.Instance(_wasm, importsObject).exports;
"#,
        bg_name,
        names_for_import_obj.join(", ")
    );

    // Add the code that initializes the `wasm` variable we declared at the top of the file.
    fixed_bindgen_glue.push_str(&initialization_glue);

    write_string_to_file(bindgen_glue_path, fixed_bindgen_glue)?;

    Ok(())
}

fn read_file_to_string<P: AsRef<Path>>(path: P) -> Result<String> {
    let file_size = path.as_ref().metadata()?.len().try_into()?;
    let mut file = File::open(path)?;
    let mut buf = Vec::with_capacity(file_size);
    file.read_to_end(&mut buf)?;
    String::from_utf8(buf).map_err(anyhow::Error::from)
}

fn write_string_to_file<P: AsRef<Path>>(path: P, contents: String) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(contents.as_bytes())?;

    Ok(())
}

// TODO: Potentially rewrite this to run an actual JavaScript parser.
fn find_exports(js: &str) -> Vec<&str> {
    const FUNCTION_EXPORT_PREFIXES: [&str; 2] = ["export function ", "export const "];

    js.lines()
        .filter_map(|line| {
            for prefix in FUNCTION_EXPORT_PREFIXES {
                if line.starts_with(prefix) {
                    let name_end_index = line
                        .chars()
                        .skip(prefix.len())
                        .take_while(|c| *c != '(' && *c != ' ')
                        .count();
                    return Some(&line[prefix.len()..name_end_index + prefix.len()]);
                }
            }

            None
        })
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::find_exports;
    use wasm_bindgen_cli_support::Bindgen;

    #[test]
    fn all_exports_found() {
        let input_path = Path::new(
            "./bindgen-test-subject/target/wasm32-unknown-unknown/release/bindgen_test_subject.wasm",
        );
        assert!(
            input_path.exists(),
            "The bindgen-test-subject module is not present, did you forget to compile it?"
        );

        let bindings = Bindgen::new()
            .input_path(input_path)
            .typescript(false)
            .generate_output()
            .expect("could not generate wasm bindings");
        let export_names = find_exports(bindings.js());

        // Ensure that every import in the final wasm binary can be found in the names we collected
        // from the generated JavaScript.
        for import in bindings.wasm().imports.iter() {
            if !export_names.iter().any(|name| &import.name == name) {
                panic!(
                    "No matching name was found for module: \"{}\" import: \"{}\"\nexports: {:?}",
                    import.module, import.name, &export_names
                )
            }
        }
    }
}
