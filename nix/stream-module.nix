# The screen-share stream host (#143): the static watch and share pages with
# the plan's isolation headers, no cookies and no access log.
# `services.nuschl-stream`. The SFU (and a /sfu location) comes with its package.
{ config, lib, ... }:
let
  cfg = config.services.nuschl-stream;
  csp = lib.concatStringsSep "; " [
    "default-src 'none'"
    "script-src 'self' 'wasm-unsafe-eval'"
    "worker-src 'self'"
    "connect-src 'self'"
    "img-src 'self' blob:"
    "style-src 'self'"
    "base-uri 'none'"
    "form-action 'none'"
    "frame-ancestors 'none'"
    "require-trusted-types-for 'script'"
  ];
  # No identity code runs here, so passkeys are denied too. nginx drops
  # inherited add_header lines in a block with its own, so every location
  # repeats the full set.
  headers = displayCapture: ''
    add_header Content-Security-Policy "${csp}" always;
    add_header Cross-Origin-Opener-Policy same-origin always;
    add_header Cross-Origin-Embedder-Policy require-corp always;
    add_header Cross-Origin-Resource-Policy same-origin always;
    add_header Origin-Agent-Cluster "?1" always;
    add_header Permissions-Policy "display-capture=${displayCapture}, camera=(), microphone=(), geolocation=(), publickey-credentials-get=(), publickey-credentials-create=()" always;
    add_header Referrer-Policy no-referrer always;
    add_header X-Content-Type-Options nosniff always;
    ${lib.optionalString cfg.forceSSL ''add_header Strict-Transport-Security "max-age=31536000" always;''}
  '';
in
{
  options.services.nuschl-stream = {
    enable = lib.mkEnableOption "the Nuschl screen-share stream host";
    hostName = lib.mkOption {
      type = lib.types.str;
      example = "stream.nuschl.eu";
      description = "The virtual host, separate from the app and identity origins.";
    };
    enableACME = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Get a certificate for hostName (security.acme must be configured).";
    };
    forceSSL = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Redirect HTTP to HTTPS.";
    };
    site = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "watch.html (served at /watch), share.html (at /share) and their assets; null serves only 404s.";
    };
  };

  config = lib.mkIf cfg.enable {
    services.nginx = {
      enable = true;
      virtualHosts.${cfg.hostName} = {
        inherit (cfg) enableACME forceSSL;
        root = cfg.site;
        extraConfig = "access_log off;";
        locations =
          if cfg.site == null then
            {
              "/" = {
                return = "404";
                extraConfig = headers "()";
              };
            }
          else
            {
              "/" = {
                tryFiles = "$uri =404";
                extraConfig = headers "()";
              };
              "= /watch" = {
                tryFiles = "/watch.html =404";
                extraConfig = headers "()";
              };
              "= /share" = {
                tryFiles = "/share.html =404";
                extraConfig = headers "(self)";
              };
            };
      };
    };
  };
}
