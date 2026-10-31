# The ccdash hub: the one process that keeps every machine's Claude Code
# history and state (sessions, the prompt history: `sync.directory`), the
# workspace repositories (`workspace.enable`: one bare repository per unit,
# served over git smart HTTP at `/git/`), serves the web UI and API, and
# sends Web Push notifications (the VAPID key pair is generated into the
# state directory on first start). Collectors (the home-manager module)
# connect to its `/collect`, `/sync/` and `/git/` endpoints, each with its
# own token.
{ localFlake }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib)
    getExe
    literalExpression
    mkDefault
    mkEnableOption
    mkIf
    mkMerge
    mkOption
    types
    ;

  cfg = config.services.ccdash-hub;
  toml = pkgs.formats.toml { };
  configFile = toml.generate "ccdash-hub.toml" cfg.settings;
  exe = getExe cfg.package;
  stateDir = "/var/lib/ccdash-hub";
  proxyEndpoint = "127.0.0.1:${toString cfg.port}";
in
{
  options.services.ccdash-hub = {
    enable = mkEnableOption "the ccdash hub";

    package = mkOption {
      type = types.package;
      default = localFlake.packages.${pkgs.stdenv.hostPlatform.system}.ccdash;
      defaultText = literalExpression "ccdash.packages.\${system}.ccdash";
      description = "The ccdash package.";
    };

    webPackage = mkOption {
      type = types.package;
      default = localFlake.packages.${pkgs.stdenv.hostPlatform.system}.ccdash-web;
      defaultText = literalExpression "ccdash.packages.\${system}.ccdash-web";
      description = "The built panel the hub serves at `/` (`http.web_dir`).";
    };

    port = mkOption {
      type = types.port;
      default = 7837;
      description = ''
        Loopback port of the HTTP API and web UI; put a TLS reverse proxy
        in front of it ({option}`nginx`).
      '';
    };

    tokenFile = mkOption {
      type = types.str;
      example = "/run/agenix/ccdash-ui-token";
      description = ''
        File holding the bootstrap bearer token of the web UI and API,
        readable by the `ccdash` user. Devices paired through a link get
        tokens of their own (Settings › Devices); the hub accepts this one
        alongside them, so changing it signs out only the devices still on
        it.
      '';
    };

    collectors = mkOption {
      type = types.attrsOf types.str;
      default = { };
      example = literalExpression ''
        {
          desk = "/run/agenix/ccdash-collector-desk";
          laptop = "/run/agenix/ccdash-collector-laptop";
        }
      '';
      description = ''
        Collector token files by host name: the name each collector
        announces, and the file holding the token it must send. Readable
        by the `ccdash` user.
      '';
    };

    settings = mkOption {
      inherit (toml) type;
      default = { };
      example = literalExpression ''
        {
          workspaces.dirs = [ "/home/me/code/work" ];
          store.retention_days = 90;
          notify.workspaces = [ "work" ];
        }
      '';
      description = ''
        Hub configuration beyond the options here, written to the TOML the
        daemon reads. Unset keys keep their built-in defaults.
      '';
    };

    nginx = {
      enable = mkEnableOption ''
        an nginx virtual host proxying to the hub, WebSocket and
        server-sent events included. Listening addresses, TLS and access
        control are yours to add on
        {option}`services.nginx.virtualHosts.<domain>`
      '';

      domain = mkOption {
        type = types.str;
        example = "ccdash.example.org";
        description = "The virtual host's name, also the hub's public URL.";
      };
    };

    sync.directory = mkOption {
      type = types.str;
      default = "${stateDir}/claude";
      description = ''
        Where the hub keeps the collectors' Claude Code state (sessions,
        rewind checkpoints, plans, tasks, pasted content), as plain files.
        Not in the nightly database copies ({option}`backup`): put it in a
        file-level backup of its own.
      '';
    };

    workspace = {
      enable = mkEnableOption ''
        the workspace sync: every collector's workspace root (its
        repositories and plain directories) kept in step through bare
        repositories under `sync.directory/workspace/`, pushed and fetched
        over git smart HTTP at `/git/` with the collector tokens. Adds git to
        the hub's closure, the nginx location with unlimited bodies, and a
        nightly `git gc` over the repositories (next to the database copy
        when {option}`backup.enable` is on)
      '';

      gcOnCalendar = mkOption {
        type = types.str;
        default = "*-*-* 03:30:00";
        description = "When to run `git gc` over the repositories (`systemd.time(7)`).";
      };
    };

    backup = {
      enable = mkEnableOption ''
        a nightly copy of the database through SQLite's backup API, checked
        with `PRAGMA integrity_check`, kept in {option}`directory` for a
        file-level backup tool (restic, say) to pick up
      '';

      directory = mkOption {
        type = types.str;
        default = "${stateDir}/backups";
        description = "Where the copies go.";
      };

      keep = mkOption {
        type = types.ints.positive;
        default = 7;
        description = "How many daily copies to keep.";
      };

      onCalendar = mkOption {
        type = types.str;
        default = "*-*-* 03:30:00";
        description = "When to copy (`systemd.time(7)`).";
      };
    };
  };

  config = mkIf cfg.enable (mkMerge [
    {
      services.ccdash-hub.settings = {
        http = {
          listen = mkDefault [ proxyEndpoint ];
          token_file = mkDefault cfg.tokenFile;
          public_url = mkIf cfg.nginx.enable (mkDefault "https://${cfg.nginx.domain}");
          web_dir = mkDefault "${cfg.webPackage}";
        };
        collect.tokens = cfg.collectors;
        store.path = mkDefault "${stateDir}/ccdash.db";
        push.key_file = mkDefault "${stateDir}/vapid.p8";
        sync.dir = mkDefault cfg.sync.directory;
        workspace.enable = mkDefault cfg.workspace.enable;
      };

      users.users.ccdash = {
        isSystemUser = true;
        group = "ccdash";
        description = "ccdash hub";
      };
      users.groups.ccdash = { };

      environment.systemPackages = [ cfg.package ];
      environment.etc."ccdash/hub.toml".source = configFile;

      systemd.services.ccdash-hub = {
        description = "ccdash hub";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        restartTriggers = [ configFile ];
        # NOTE: the hub spawns `git http-backend` and the pre-receive guard
        path = lib.optional cfg.workspace.enable pkgs.git;
        serviceConfig = {
          ExecStart = "${exe} hub --config ${configFile}";
          User = "ccdash";
          Group = "ccdash";
          StateDirectory = "ccdash-hub";
          StateDirectoryMode = "0700";
          ReadWritePaths = mkIf (!lib.hasPrefix "${stateDir}/" cfg.sync.directory) [
            cfg.sync.directory
          ];
          Restart = "on-failure";
          RestartSec = 2;
          # hardening
          NoNewPrivileges = true;
          PrivateTmp = true;
          PrivateDevices = true;
          ProtectSystem = "strict";
          ProtectHome = true;
          ProtectKernelTunables = true;
          ProtectKernelModules = true;
          ProtectControlGroups = true;
          RestrictAddressFamilies = [
            "AF_INET"
            "AF_INET6"
            "AF_UNIX"
          ];
          LockPersonality = true;
          MemoryDenyWriteExecute = true;
        };
      };
    }
    (mkIf cfg.nginx.enable {
      services.nginx.enable = true;
      services.nginx.virtualHosts.${cfg.nginx.domain}.locations = {
        "/" = {
          proxyPass = "http://${proxyEndpoint}";
          proxyWebsockets = true;
          # NOTE: SSE streams and collector links stay open for hours
          extraConfig = ''
            proxy_buffering off;
            proxy_read_timeout 1h;
            proxy_send_timeout 1h;
          '';
        };
        # NOTE: collectors move files in pieces of up to 8 MiB
        "/sync/" = {
          proxyPass = "http://${proxyEndpoint}";
          extraConfig = ''
            client_max_body_size 16m;
            proxy_request_buffering off;
            proxy_read_timeout 5m;
            proxy_send_timeout 5m;
          '';
        };
        # NOTE: git pushes stream a pack of any size; a seed takes minutes
        "/git/" = mkIf cfg.workspace.enable {
          proxyPass = "http://${proxyEndpoint}";
          extraConfig = ''
            client_max_body_size 0;
            proxy_request_buffering off;
            proxy_buffering off;
            proxy_read_timeout 30m;
            proxy_send_timeout 30m;
          '';
        };
      };
    })
    (mkIf cfg.workspace.enable {
      systemd.services.ccdash-hub-gc = {
        description = "ccdash hub: git gc over the workspace repositories";
        after = [ "ccdash-hub.service" ];
        path = [ pkgs.git ];
        script = "${exe} gc --config ${configFile}";
        serviceConfig = {
          Type = "oneshot";
          User = "ccdash";
          Group = "ccdash";
          StateDirectory = "ccdash-hub";
          StateDirectoryMode = "0700";
          ReadWritePaths = mkIf (!lib.hasPrefix "${stateDir}/" cfg.sync.directory) [
            cfg.sync.directory
          ];
          Nice = 10;
          IOSchedulingClass = "idle";
        };
      };
      systemd.timers.ccdash-hub-gc = {
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnCalendar = cfg.workspace.gcOnCalendar;
          Persistent = true;
          RandomizedDelaySec = "20m";
        };
      };
    })
    (mkIf cfg.backup.enable {
      systemd.services.ccdash-hub-backup = {
        description = "ccdash hub database copy";
        after = [ "ccdash-hub.service" ];
        path = [ pkgs.findutils ];
        script = ''
          dir=${lib.escapeShellArg cfg.backup.directory}
          mkdir -p "$dir"
          ${exe} backup --config ${configFile} "$dir/ccdash-$(date +%F).db"
          find "$dir" -name 'ccdash-*.db' -type f -printf '%T@ %p\n' | sort -rn \
            | tail -n +$((${toString cfg.backup.keep} + 1)) | cut -d' ' -f2- | xargs -r rm --
          # NOTE: earlier copies left WAL side files and were world-readable
          find "$dir" -name 'ccdash-*.db.partial-*' -type f -delete
          chmod -R go-rwx "$dir"
        '';
        serviceConfig = {
          Type = "oneshot";
          User = "ccdash";
          Group = "ccdash";
          StateDirectory = "ccdash-hub";
          # NOTE: the hub's mode; without it each run resets the state dir to 0755
          StateDirectoryMode = "0700";
          UMask = "0077";
        };
      };
      systemd.timers.ccdash-hub-backup = {
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnCalendar = cfg.backup.onCalendar;
          Persistent = true;
          RandomizedDelaySec = "10m";
        };
      };
    })
  ]);
}
