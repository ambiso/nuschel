# Offline maintainer recipe. Generate into a new Nix output, never the checkout:
# nix build --impure --file nix/syntax-bundle.nix --cores 2 --max-jobs 1
# Review/compare result/default-nix.packdump before explicitly copying it.
let
  projectRoot = ../.;
  flake = builtins.getFlake (toString projectRoot);
  system = builtins.currentSystem;
  pkgs = import flake.inputs.nixpkgs {
    inherit system;
    overlays = [ (import flake.inputs.rust-overlay) ];
  };
  toolchain = pkgs.rust-bin.stable.latest.default.override {
    extensions = [ "rust-src" ];
    targets = [ "wasm32-unknown-unknown" ];
  };
  project = (import ./crate2nix.nix { inherit pkgs toolchain; }).project;
  # Reuse the locked graph's EXISTING generation-capable Syntect configuration.
  # This does not change the normal content dependency's enabled features.
  features = project.internal.crates.syntect.resolvedDefaultFeatures;
  syntect = project.internal.buildRustCrateWithFeatures {
    packageId = "syntect";
    inherit features;
  };
  source = pkgs.runCommand "nuschl-syntax-generator-source" { } ''
    mkdir -p "$out/src"
    cp ${../tools/generate-syntaxes.rs} "$out/src/main.rs"
    cat > "$out/Cargo.toml" <<'EOF'
    [package]
    name = "nuschl-syntax-bundle-generator"
    version = "0.1.0"
    edition = "2024"
    license = "EUPL-1.2"
    [dependencies]
    syntect = "=5.3.0"
    EOF
  '';
  generator =
    (pkgs.buildRustCrate.override {
      rustc = toolchain;
      cargo = toolchain;
    })
      {
        crateName = "nuschl-syntax-bundle-generator";
        version = "0.1.0";
        edition = "2024";
        src = source;
        dependencies = [ syntect ];
        crateBin = [
          {
            name = "generate-syntaxes";
            path = "src/main.rs";
          }
        ];
      };
in
assert builtins.elem "yaml-load" features;
assert !(builtins.elem "regex-onig" features);
pkgs.runCommand "nuschl-content-syntax-bundle" { } ''
  ${generator}/bin/generate-syntaxes \
    ${../release/licenses/syntect-assets/nix/Nix.sublime-syntax} "$out"
''
