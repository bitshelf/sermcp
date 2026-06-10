//! Build script: read `[package.metadata.learn]` from Cargo.toml and
//! expose as `DUT_CTRL_*` env vars for `env!()` in source code.

use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let toml_path = Path::new(&manifest_dir).join("Cargo.toml");
    let content = std::fs::read_to_string(&toml_path).expect("Cannot read Cargo.toml");

    let config: toml::Value = toml::from_str(&content).expect("Cannot parse Cargo.toml");

    let learn = config
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("learn"));

    if let Some(learn) = learn {
        emit_if_present(learn, "min_reset_ms", "DUT_CTRL_MIN_RESET_MS");
    } else {
        println!("cargo:rustc-env=DUT_CTRL_MIN_RESET_MS=3000");
    }
}

fn emit_if_present(table: &toml::Value, key: &str, env_name: &str) {
    if let Some(val) = table.get(key) {
        match val {
            toml::Value::Integer(i) => println!("cargo:rustc-env={env_name}={i}"),
            toml::Value::Float(f) => println!("cargo:rustc-env={env_name}={f}"),
            toml::Value::String(s) => println!("cargo:rustc-env={env_name}={s}"),
            _ => {}
        }
    }
}
