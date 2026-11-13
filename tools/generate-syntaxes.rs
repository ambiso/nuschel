// SPDX-License-Identifier: EUPL-1.2
//! Offline maintainer tool; built by nix/syntax-bundle.nix, not a workspace target.
//! Outputs a NEW directory only. Copy a reviewed result explicitly; this tool
//! never overwrites a checked-in asset or downloads syntax definitions.
#![forbid(unsafe_code)]
use std::{
    env, fs,
    io::{Read, Write},
    path::Path,
};
use syntect::{
    dumps,
    parsing::{SyntaxDefinition, SyntaxSet},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("expected: generate-syntaxes RAW_NIX_YAML NEW_OUTPUT_DIRECTORY".into());
    }
    let mut source = String::new();
    fs::File::open(&args[0])?
        .take(65537)
        .read_to_string(&mut source)?;
    if source.len() > 65536 {
        return Err("syntax source exceeds maintainer input limit".into());
    }
    // A compiled foreign SyntaxDefinition retains Direct ContextIds. Loading
    // the raw YAML gives symbolic references that this builder can link safely.
    let nix = SyntaxDefinition::load_from_str(&source, true, Some("Nix"))?;
    if nix.name != "Nix" || nix.scope.to_string() != "source.nix" || nix.file_extensions != ["nix"]
    {
        return Err("unexpected grammar identity".into());
    }
    let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
    let original = builder.syntaxes().to_vec();
    if original.iter().any(|syntax| syntax.name == "Nix") {
        return Err("defaults already contain Nix; review the bundle recipe".into());
    }
    builder.add(nix);
    let combined = builder.build();
    if !combined.find_unlinked_contexts().is_empty() {
        return Err("bundle has unresolved syntax contexts".into());
    }
    let reconstructed = combined.clone().into_builder();
    if &reconstructed.syntaxes()[..original.len()] != original.as_slice() {
        return Err("adding Nix changed an existing syntax definition".into());
    }
    let output = Path::new(&args[1]);
    fs::create_dir(output)?;
    dumps::dump_to_uncompressed_file(&combined, output.join("default-nix.packdump"))?;
    let mut report = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("generation.txt"))?;
    writeln!(
        report,
        "Syntect 5.3.0 defaults plus pinned Nix YAML; newlines enabled\nExisting definitions unchanged: {}\nTotal definitions: {}\nUnlinked contexts: 0",
        original.len(),
        combined.syntaxes().len()
    )?;
    Ok(())
}
