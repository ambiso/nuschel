# Source/checksum/exact offline-closure proof only; no compilation/publication.
{
  pkgs,
  toolchain,
  sourcePackage,
  version,
  debianRevision ? "1",
}:
pkgs.runCommand "nuschl-debian-source-check-${version}"
  {
    nativeBuildInputs = [
      pkgs.python3
      pkgs.dpkg
      toolchain
    ];
  }
  ''
    export HOME="$TMPDIR/home" CARGO_HOME="$TMPDIR/cargo-home" CARGO_NET_OFFLINE=true
    export RUSTC=${toolchain}/bin/rustc
    mkdir -p "$HOME" "$CARGO_HOME" "$out"
    dpkg-source --extract ${sourcePackage}/nuschl_${version}-${debianRevision}.dsc source
    cd source
    sha256sum Cargo.lock > before-lock.sha256
    cargo metadata --frozen --offline --format-version 1 > metadata.json
    sha256sum --check before-lock.sha256
    export NUSCHL_SOURCE_PACKAGE=${sourcePackage}
    python3 -c ${pkgs.lib.escapeShellArg ''
      import hashlib, json, os
      from pathlib import Path
      root = Path.cwd().resolve()
      metadata = json.loads(Path('metadata.json').read_text())
      packages = {package['id']: package for package in metadata['packages']}
      roots = {packages[key]['name'] for key in metadata['workspace_members']}
      required = {'nuschl', 'nuschl-content-worker', 'nuschl-crypto-worker', 'nuschl-math-worker',
                  'nuschl-media-worker', 'nuschl-storage-worker', 'nuschl-transfer-worker', 'nuschl-update-worker'}
      assert required <= roots
      assert all(Path(package['manifest_path']).resolve().is_relative_to(root) for package in packages.values())
      report = {'source_package': os.environ['NUSCHL_SOURCE_PACKAGE'], 'extraction_checksums': True,
                'unsigned_source': True, 'cargo_metadata_frozen_offline': True,
                'cargo_lock_sha256': hashlib.sha256(Path('Cargo.lock').read_bytes()).hexdigest(),
                'package_count': len(packages), 'required_client_helper_roots': sorted(required),
                'all_manifests_inside_upload': True, 'rust_crate_compilation': False,
                'scope': 'Source unpack/locked offline metadata; not a distro source rebuild, signing, legal completeness or runtime proof.'}
      Path(os.environ['out'], 'proof.json').write_text(json.dumps(report, indent=2) + '\n')
    ''}
  ''
