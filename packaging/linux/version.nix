# Pure exported-source policy. Version eligibility is not a publication approval.
{
  baseVersion,
  sourceInfo,
  metadata ? null,
}:
let
  check = condition: message: if condition then true else throw "Nuschl distro version: ${message}";
  string = value: maximum: builtins.isString value && builtins.stringLength value <= maximum;
  nar = value: string value 60 && builtins.match "sha256-[A-Za-z0-9+/]{43}=" value != null;
  revision = value: string value 40 && builtins.match "[0-9a-f]{40}" value != null;
  actualNar = sourceInfo.narHash or null;
  knownRevision = sourceInfo.rev or null;
  keys = [
    "base_version"
    "commit_epoch"
    "commit_stamp"
    "kind"
    "package_revision"
    "schema"
    "sequence"
    "source_nar_hash"
    "source_revision"
    "tag"
  ];
  bound =
    if metadata == null then
      null
    else
      assert check (builtins.isAttrs metadata) "metadata must be an object";
      assert check (builtins.attrNames metadata == keys) "unknown or missing schema1 fields";
      assert check (metadata.schema == 1) "unsupported schema";
      assert check (metadata.base_version == baseVersion) "workspace version mismatch";
      assert check (metadata.kind == "release" || metadata.kind == "development") "invalid kind";
      assert check (
        nar metadata.source_nar_hash && metadata.source_nar_hash == actualNar
      ) "source NarHash mismatch";
      assert check (revision metadata.source_revision) "invalid source revision";
      assert check (
        knownRevision == null || metadata.source_revision == knownRevision
      ) "source revision mismatch";
      assert check (
        builtins.isInt metadata.commit_epoch
        && metadata.commit_epoch >= 0
        && metadata.commit_epoch <= 253402300799
      ) "invalid epoch";
      assert check (
        string metadata.commit_stamp 14
        &&
          builtins.match "[0-9]{4}(0[1-9]|1[0-2])(0[1-9]|[12][0-9]|3[01])([01][0-9]|2[0-3])[0-5][0-9][0-5][0-9]" metadata.commit_stamp
          != null
      ) "invalid fixed UTC stamp";
      assert check (
        builtins.isInt metadata.package_revision
        && metadata.package_revision > 0
        && metadata.package_revision <= 999999
      ) "invalid package revision";
      assert check (
        if metadata.kind == "release" then metadata.tag == "v${baseVersion}" else metadata.tag == null
      ) "invalid release tag policy";
      assert check (
        if metadata.kind == "development" then
          builtins.isInt metadata.sequence && metadata.sequence > 0 && metadata.sequence <= 1000000000
        else
          metadata.sequence == null
          || (builtins.isInt metadata.sequence && metadata.sequence > 0 && metadata.sequence <= 1000000000)
      ) "invalid development sequence";
      metadata;
  official = bound != null;
  localIdentity = builtins.substring 0 16 (builtins.hashString "sha256" actualNar);
  suffix =
    if official && bound.kind == "development" then
      "${toString bound.sequence}.${bound.commit_stamp}.g${builtins.substring 0 12 bound.source_revision}"
    else
      null;
  debianUpstream =
    if !official then
      "${baseVersion}~local.n${localIdentity}"
    else if bound.kind == "release" then
      baseVersion
    else
      "${baseVersion}+git${suffix}";
  archPkgver =
    if !official then
      "${baseVersion}local.n${localIdentity}"
    else if bound.kind == "release" then
      baseVersion
    else
      "${baseVersion}.r${suffix}";
  packageRevision = if official then toString bound.package_revision else "1";
in
assert check (
  string baseVersion 100 && builtins.match "[0-9]+\\.[0-9]+\\.[0-9]+" baseVersion != null
) "invalid base version";
assert check (
  builtins.isAttrs sourceInfo && nar actualNar
) "actual unfiltered sourceInfo.narHash is required";
assert check (knownRevision == null || revision knownRevision) "invalid sourceInfo revision";
{
  inherit
    baseVersion
    debianUpstream
    archPkgver
    packageRevision
    ;
  debianVersion = "${debianUpstream}-${packageRevision}";
  archPkgrel = packageRevision;
  archVersion = "${archPkgver}-${packageRevision}";
  officialVersion = official;
  publicationReady = false; # Legal, platform/install and signing gates are separate.
  kind = if official then bound.kind else "local";
  sourceNarHash = actualNar;
  sourceRevision = if official then bound.source_revision else knownRevision;
  sourceDateEpoch = if official then bound.commit_epoch else 1;
  sourceMetadata = bound;
}
