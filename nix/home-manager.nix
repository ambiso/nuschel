# Home Manager module for the Nuschl desktop client. It does not depend on
# this flake's nixpkgs: only the package comes from the flake, chosen for the
# consumer's platform and replaceable through `programs.nuschl.package`.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib) mkOption types;
  cfg = config.programs.nuschl;
  json = pkgs.formats.json { };
  inherit (pkgs.stdenv.hostPlatform) isLinux isDarwin system;

  # Nuschl's configuration folder (`Layout::system` in
  # crates/session/src/layout.rs): $XDG_CONFIG_HOME/nuschl, which holds only
  # settings.json, servers.json (bookmarks) and managed.json. The identity,
  # keys and history are in $XDG_DATA_HOME/nuschl, so a dotfiles repository
  # of ~/.config never sees them.
  configDir =
    if isDarwin then
      "${config.home.homeDirectory}/Library/Application Support/org.Nuschl.Nuschl"
    else
      "${config.xdg.configHome}/nuschl";

  # The entry the in-app switch writes (apps/desktop/src/autostart.rs), under
  # the same file name, so the app shows the switch as on and never adds a
  # second one. Exec names this generation's binary: Home Manager rewrites the
  # entry on every switch, and the session's PATH may not include the profile.
  autostartEntry = pkgs.writeTextDir "org.nuschl.Nuschl.desktop" ''
    [Desktop Entry]
    Type=Application
    Name=Nuschl
    Comment=Voice and text chat
    Exec=${lib.getExe cfg.package} --autostart
    Icon=org.nuschl.Nuschl
    Terminal=false
    X-GNOME-Autostart-enabled=true
  '';
  autostartReadOnly =
    (config.xdg.autostart.enable or false) && (config.xdg.autostart.readOnly or false);

  # What crates/session/src/config.rs accepts in servers.json and managed.json.
  # Nuschl sets aside a servers.json, and ignores a managed.json, with a
  # bookmark it rejects, so these are checked here.
  host = types.strMatching "[^][[:space:][:cntrl:]/@\\]{1,253}" // {
    description = "hostname or IP address (IPv6 without brackets), at most 253 bytes";
  };
  username =
    types.addCheck (types.strMatching "[^[:cntrl:]]{1,128}") (
      name: builtins.match "[[:space:]]*" name == null
    )
    // {
      description = "user name of at most 128 bytes without control characters";
    };
  server =
    { config, ... }:
    {
      options = {
        name = mkOption {
          type = types.strMatching "[^[:cntrl:]]{1,128}";
          default = config.host;
          defaultText = lib.literalExpression "host";
          description = "Name shown in the server list.";
        };
        host = mkOption {
          type = host;
          example = "voice.example.org";
          description = "Hostname or IP address of the Mumble server.";
        };
        port = mkOption {
          type = types.ints.between 1 65535;
          default = 64738;
          description = "Port of the Mumble server.";
        };
        username = mkOption {
          type = username;
          example = "alice";
          description = "User name to log in with.";
        };
        autoConnect = mkOption {
          type = types.bool;
          default = true;
          description = "Connect when Nuschl starts, self-muted and deafened.";
        };
        tokens = mkOption {
          type = types.listOf (types.addCheck types.str (token: lib.stringLength token <= 256));
          default = [ ];
          description = ''
            Access tokens (ACL group passwords), sent with every login.
            They are stored in the world-readable Nix store.
          '';
        };
        certificateSha256 = mkOption {
          type = types.nullOr (types.strMatching "[0-9a-fA-F]{64}");
          default = null;
          description = ''
            SHA-256 fingerprint (hexadecimal) of the server's certificate. When set,
            Nuschl accepts exactly this certificate without asking, and no other,
            including CA-signed ones. Use it for a self-signed certificate you
            verified independently. Without it, Nuschl asks on first connect.
          '';
        };
      };
    };
  # A stable UUID per endpoint and user, so a bookmark keeps its identity
  # however the list is ordered.
  bookmarkId =
    server:
    let
      h = builtins.hashString "sha256" "nuschl-home-manager:${lib.toLower server.host}:${toString server.port}:${server.username}";
      part = start: length: builtins.substring start length h;
    in
    "${part 0 8}-${part 8 4}-8${part 13 3}-a${part 17 3}-${part 20 12}";
  bookmark = server: {
    id = bookmarkId server;
    inherit (server)
      name
      port
      username
      tokens
      ;
    host = lib.toLower server.host;
    certificate_sha256 = lib.mapNullable lib.toLower server.certificateSha256;
    auto_connect = server.autoConnect;
  };

  # The fields of ClientSettings in crates/session/src/config.rs (checked by
  # nix/home-manager-test.nix). Nuschl ignores a managed.json with any other.
  settingNames = [
    "audio"
    "force_tcp"
    "auto_reconnect"
    "user_volumes"
    "local_mutes"
    "server_volumes"
    "shortcuts"
    "push_to_talk_key"
    "enter_sends"
    "sidebar_width"
    "check_updates"
    "notifications"
    "server_icons"
    "sound_effects"
    "sound_volume"
    "friends"
    "nicknames"
    "friend_notifications"
    "send_typing"
    "relay_only"
    "wormhole_rendezvous"
    "wormhole_relay"
    "appearance"
    "recovery_key"
    "e2ee_direct_messages"
    "key_transparency"
    "share_web_host"
  ];
  unknownSettings = lib.subtractLists settingNames (lib.attrNames cfg.settings);
  # `programs.nuschl.keyTransparency`, in the format of `key_transparency`.
  keyTransparency = lib.optionalAttrs (cfg.keyTransparency != null) {
    key_transparency = {
      default_witnesses = cfg.keyTransparency.defaultWitnesses;
      witnesses = map (witness: {
        inherit (witness) key enabled;
      }) cfg.keyTransparency.witnesses;
      max_age_hours = cfg.keyTransparency.maxAgeHours;
      block_new_devices = cfg.keyTransparency.blockNewDevices;
    }
    // lib.optionalAttrs (cfg.keyTransparency.required != null) {
      inherit (cfg.keyTransparency) required;
    };
  };
  managedSettings = cfg.settings // keyTransparency;
  managedFile = json.generate "nuschl-managed.json" {
    settings = managedSettings;
    servers = map bookmark cfg.servers;
  };
  managed = managedSettings != { } || cfg.servers != [ ];

  # Bookmark lists checked like Nuschl checks them.
  serverAssertions =
    option: servers:
    let
      ids = map bookmarkId servers;
    in
    [
      {
        assertion = lib.length servers <= 100;
        message = "programs.nuschl.${option}: Nuschl saves at most 100 servers.";
      }
      {
        assertion = lib.all (server: lib.length server.tokens <= 64) servers;
        message = "programs.nuschl.${option}: at most 64 access tokens per server.";
      }
      {
        assertion = lib.length ids == lib.length (lib.unique ids);
        message = "programs.nuschl.${option}: each host, port and user name may appear only once.";
      }
    ];

  initialFiles =
    lib.optionalAttrs (cfg.initialSettings != { }) {
      "settings.json" = json.generate "nuschl-settings.json" cfg.initialSettings;
    }
    // lib.optionalAttrs (cfg.initialServers != [ ]) {
      "servers.json" = json.generate "nuschl-servers.json" (map bookmark cfg.initialServers);
    };
  # Writes each file only where Nuschl has none. Nuschl creates the folder
  # owner-only and its files 0600; this does the same.
  writeInitialFiles = pkgs.writeShellScript "nuschl-initial-files" ''
    set -eu
    PATH=${lib.makeBinPath [ pkgs.coreutils ]}
    directory=$1
    install -d -m 0700 -- "$directory"
    ${lib.concatStrings (
      lib.mapAttrsToList (name: file: ''
        if [ ! -e "$directory/${name}" ] && [ ! -L "$directory/${name}" ]; then
          install -m 0600 -- ${file} "$directory/.${name}.home-manager"
          mv -f -- "$directory/.${name}.home-manager" "$directory/${name}"
          echo "Nuschl: wrote $directory/${name}"
        fi
      '') initialFiles
    )}
  '';
in
{
  imports = [ (lib.mkRenamedOptionModule [ "programs" "nuschel" ] [ "programs" "nuschl" ]) ];
  options.programs.nuschl = {
    enable = lib.mkEnableOption "Nuschl, a Mumble-compatible voice and text chat client";

    package = mkOption {
      type = types.package;
      default = self.packages.${system}.default;
      defaultText = lib.literalExpression "inputs.nuschl.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "The Nuschl package. Its desktop entry and icons are installed with it.";
    };

    autostart = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Start Nuschl minimised when you log in (Linux), through the XDG
        autostart entry that **Settings → General** would write. The entry
        replaces one the app wrote earlier. Turning the switch off in the app
        removes it until the next `home-manager switch`. When this is
        `false`, the module leaves the entry and the in-app switch alone.
      '';
    };

    initialSettings = mkOption {
      type = types.attrsOf json.type;
      default = { };
      example = lib.literalExpression ''
        {
          enter_sends = false;
          push_to_talk_key = "f13";
          audio = {
            transmit = "push_to_talk";
            echo_cancellation = "auto";
          };
        }
      '';
      description = ''
        Settings written to Nuschl's `settings.json`, in its own format
        (`ClientSettings` in crates/session/src/config.rs; keys you leave out
        keep their defaults).

        The file is written only if it does not exist yet, typically before
        Nuschl's first start. Nuschl owns it after that: changes here no
        longer apply, and Nuschl overwrites it when you change a setting.
        A key Nuschl does not know makes it set the file aside (as
        `settings.json.invalid`) and start with default settings. For
        settings that always apply, use {option}`programs.nuschl.settings`.
      '';
    };

    initialServers = mkOption {
      type = types.listOf (types.submodule server);
      default = [ ];
      example = lib.literalExpression ''
        [
          {
            name = "Team";
            host = "voice.example.org";
            username = "alice";
          }
        ]
      '';
      description = ''
        Server bookmarks written to Nuschl's `servers.json`, only if the file
        does not exist yet. Like {option}`programs.nuschl.initialSettings`,
        they seed a new profile: Nuschl owns the file after that. For
        bookmarks that always apply, use {option}`programs.nuschl.servers`.
      '';
    };

    settings = mkOption {
      type = types.attrsOf json.type;
      default = { };
      example = lib.literalExpression ''
        {
          enter_sends = false;
          shortcuts."push-to-talk" = "f13";
          audio.transmit = "push_to_talk";
        }
      '';
      description = ''
        Settings that apply on every start, in the format of
        {option}`programs.nuschl.initialSettings`. They are written to a
        read-only `managed.json` beside Nuschl's own files, merged key by key
        over `settings.json` (objects such as `audio` merge too). Nuschl
        shows them in Settings as set by your system configuration, does not
        let you change them, and never writes them to `settings.json`, so your
        own choices return when you remove a setting here. Unknown top-level
        names fail evaluation; Nuschl reports any other invalid value and then
        ignores `managed.json` until it is fixed.
      '';
    };

    keyTransparency = mkOption {
      type = types.nullOr (
        types.submodule {
          options = {
            defaultWitnesses = mkOption {
              type = types.bool;
              default = true;
              description = "Require Nuschl's built-in witnesses (the project auditor; none until it is deployed).";
            };
            witnesses = mkOption {
              type = types.listOf (
                types.submodule {
                  options = {
                    key = mkOption {
                      type = types.strMatching "[^+[:space:]]{1,255}\\+[0-9a-f]{8}\\+[A-Za-z0-9+/=]+";
                      example = "witness.example.org/nuschl-kt+122b83c3+BuajLAZT…";
                      description = "The witness's verifier key (`nuschl-witness vkey`).";
                    };
                    enabled = mkOption {
                      type = types.bool;
                      default = true;
                      description = "False turns a witness off, also a built-in one.";
                    };
                  };
                }
              );
              default = [ ];
              description = "Further witnesses every server's key-transparency log must be cosigned by.";
            };
            required = mkOption {
              type = types.nullOr (types.ints.between 1 16);
              default = null;
              description = ''
                How many of the enabled witnesses must have cosigned; null: all of them.
                Every group of clients that requires k of the same n witnesses is safe
                from split views only if 2k > n.
              '';
            };
            maxAgeHours = mkOption {
              type = types.ints.between 1 168;
              default = 24;
              description = "How old the newest cosignatures may be (a frozen view lasts at most this long).";
            };
            blockNewDevices = mkOption {
              type = types.bool;
              default = true;
              description = ''
                While a server's log is not witnessed ("Automatic verification
                unavailable"), do not add contacts' new devices to existing end-to-end
                conversations. First contact proceeds either way, as "not verified".
              '';
            };
          };
        }
      );
      default = null;
      example = lib.literalExpression ''
        {
          witnesses = [ { key = "witness.example.org/nuschl-kt+122b83c3+BuajLAZT…"; } ];
          maxAgeHours = 24;
        }
      '';
      description = ''
        Key-transparency witnesses (docs/witness.md), written to `managed.json` as
        `key_transparency` and shown as set by your system configuration. Takes
        precedence over `settings.key_transparency`.
      '';
    };

    servers = mkOption {
      type = types.listOf (types.submodule server);
      default = [ ];
      example = lib.literalExpression ''
        [
          {
            name = "Team";
            host = "voice.example.org";
            username = "alice";
          }
        ]
      '';
      description = ''
        Server bookmarks that apply on every start, written to `managed.json`.
        Nuschl lists them with your own bookmarks but does not let you remove
        them or change their name, address, username, tokens or automatic
        connection. It keeps what it learns about them (an accepted
        certificate, the channel tree, the last channel) in `servers.json`.
        A bookmark removed here disappears from Nuschl on its next start.
        A bookmark here with the host, port and username of one in
        {option}`programs.nuschl.initialServers` takes that one over.
      '';
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        assertions = [
          {
            assertion = !cfg.autostart || isLinux;
            message = "programs.nuschl.autostart is supported on Linux only; on macOS use Nuschl's Settings → General.";
          }
          {
            assertion = unknownSettings == [ ];
            message = "programs.nuschl.settings: Nuschl has no settings named ${lib.concatStringsSep ", " unknownSettings}.";
          }
        ]
        ++ serverAssertions "initialServers" cfg.initialServers
        ++ serverAssertions "servers" cfg.servers;
        home.packages = [ cfg.package ];
      }
      (lib.mkIf (cfg.autostart && isLinux && !autostartReadOnly) {
        xdg.configFile."autostart/org.nuschl.Nuschl.desktop" = {
          source = "${autostartEntry}/org.nuschl.Nuschl.desktop";
          # The app writes an equivalent file here; replace it.
          force = true;
        };
      })
      # A read-only autostart folder takes its entries from this list.
      (lib.mkIf (cfg.autostart && isLinux && autostartReadOnly) {
        xdg.autostart.entries = [ "${autostartEntry}/org.nuschl.Nuschl.desktop" ];
      })
      # Read-only: Nuschl never writes managed.json.
      (lib.mkIf (managed && !isDarwin) {
        xdg.configFile."nuschl/managed.json".source = managedFile;
      })
      (lib.mkIf (managed && isDarwin) {
        home.file."Library/Application Support/org.Nuschl.Nuschl/managed.json".source = managedFile;
      })
      (lib.mkIf (initialFiles != { }) {
        home.activation.nuschlInitialFiles = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
          run ${writeInitialFiles} ${lib.escapeShellArg configDir}
        '';
      })
    ]
  );
}
