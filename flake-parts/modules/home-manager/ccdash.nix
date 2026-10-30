# --- flake-parts/modules/home-manager/ccdash.nix
{ localFlake }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib)
    elem
    genAttrs
    getExe
    hiPrio
    literalExpression
    mkDefault
    mkEnableOption
    mkForce
    mkIf
    mkMerge
    mkOption
    optionalAttrs
    optionals
    optionalString
    types
    ;

  cfg = config.services.ccdash;
  claudeCode = config.programs.claude-code;
  toml = pkgs.formats.toml { };
  configFile = toml.generate "ccdash-config.toml" cfg.settings;
  exe = getExe cfg.package;
  statusLineCommand = cfg.claudeCode.statusLine.command;

  hookEvents = [
    "SessionStart"
    "SessionEnd"
    "UserPromptSubmit"
    "PreToolUse"
    "PostToolUse"
    "PostToolUseFailure"
    "Notification"
    "PermissionRequest"
    "PermissionDenied"
    "Stop"
    "StopFailure"
    "PostCompact"
    "SubagentStart"
    "SubagentStop"
    "TaskCreated"
    "TaskCompleted"
    "MessageDisplay"
  ];
  toolEvents = [
    "PreToolUse"
    "PostToolUse"
    "PostToolUseFailure"
    "PermissionRequest"
    "PermissionDenied"
  ];
  hook = {
    type = "command";
    command = "${exe} emit";
    timeout = 5;
  };
  # NOTE: holds the prompt until ccdash answers or it closes; exits early then
  askHook = {
    type = "command";
    command = "${exe} ask";
    timeout = 86400;
  };

  channel = cfg.claudeCode.channel;
  pluginDir = "${cfg.package}/share/ccdash/claude-plugin";
  # NOTE: `claude <subcommand>` parsers reject the channel flags (`rc` included)
  claudeWrapper = pkgs.writeShellScriptBin "claude" ''
    real=${getExe claudeCode.package}
    case "''${1-}" in
      agents | attach | auth | auto-mode | doctor | gateway | import | install | logs | mcp | \
        plugin | plugins | purge | rc | remote-control | respawn | rm | setup-token | stop | \
        kill | ultrareview | update | upgrade)
        exec "$real" "$@"
        ;;
    esac
    for arg in "$@"; do
      case $arg in
        -p | --print | --bg) exec "$real" "$@" ;;
      esac
    done
    exec "$real" ${
      if channel.devFlag then "--dangerously-load-development-channels" else "--channels"
    } plugin:ccdash@inline --plugin-dir ${pluginDir} "$@"
  '';
in
{
  options.services.ccdash = {
    enable = mkEnableOption ''
      the ccdash collector: reports this machine's Claude Code sessions to
      a ccdash hub (the NixOS module `services.ccdash-hub`) and carries out
      what the hub sends back
    '';

    package = mkOption {
      type = types.package;
      default = localFlake.packages.${pkgs.stdenv.hostPlatform.system}.ccdash;
      defaultText = literalExpression "ccdash.packages.\${system}.ccdash";
      description = "The ccdash package.";
    };

    hub = {
      url = mkOption {
        type = types.str;
        example = "wss://ccdash.example.org/collect";
        description = "The hub's `/collect` endpoint.";
      };

      tokenFile = mkOption {
        type = types.str;
        example = "\${XDG_RUNTIME_DIR}/agenix/ccdash-collector-token";
        description = ''
          File holding this machine's collector token, the one the hub
          lists under this host name. `$VAR` and `''${VAR}` expand.
        '';
      };
    };

    settings = mkOption {
      inherit (toml) type;
      default = { };
      example = literalExpression ''
        {
          niri.terminals = [ "com.mitchellh.ghostty" "foot" ];
          procs.enable = false;
        }
      '';
      description = ''
        Collector configuration, written to
        {file}`$XDG_CONFIG_HOME/ccdash/config.toml`. Unset keys keep their
        built-in defaults.
      '';
    };

    sync.enable = mkEnableOption ''
      keeping this machine's Claude Code state (sessions, rewind
      checkpoints, plans, tasks, pasted content, the prompt history) in step
      with the hub, so a session ended here resumes on another machine and
      the prompt history is one. The hub refuses prompts in a session that
      is open on another machine. Configure what travels under
      {option}`settings.sync` (`include`, `exclude`, `root`)
    '';

    workspace = {
      enable = mkEnableOption ''
        keeping this machine's workspace root (every git repository and plain
        directory under it, at {option}`depth`) in step with the other
        machines through the hub: snapshots of the working tree, the staged
        state, branches, tags and stashes travel as they change; the other
        machines fast-forward or merge, conflicts land as git markers plus an
        entry in the panel. The hub owns the configuration once it has one;
        the options here are the starting point (handoff's `config.toml`
        carried over). The collector's unit runs with `CPUWeight=20`,
        `IOWeight=20` and `Nice=10` so a seed or a big merge never competes
        with your work
      '';

      root = mkOption {
        type = types.str;
        default = "~/ProjectBundle";
        description = "The workspace root. `~/` and `$VAR` expand.";
      };

      depth = mkOption {
        type = types.ints.positive;
        default = 2;
        description = "Units are expected this many levels below the root (`2`: `<root>/<bundle>/<repo>`).";
      };

      include = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [
          "work/*"
          "me/*"
        ];
        description = "Globs on unit ids (`<bundle>/<repo>`) that travel; empty includes every unit.";
      };

      exclude = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [
          "me/nixpkgs"
          "*_bkp"
        ];
        description = "Globs on unit ids that never travel; wins over `include`.";
      };

      alwaysExclude = mkOption {
        type = types.listOf types.str;
        default = [
          "node_modules"
          ".venv"
          "venv"
          ".direnv"
          ".devenv"
          "target"
          "result"
          "result-*"
          "__pycache__"
          ".cache"
        ];
        description = "Directory names that never travel, wherever they appear, unless {option}`syncIgnored` names them for a unit.";
      };

      syncIgnored = mkOption {
        type = types.listOf types.str;
        default = [
          ".env"
          ".env.*"
          ".envrc.local"
        ];
        description = "Gitignored files and directories (paths relative to the unit, a glob in the last segment) that travel anyway; paths under `.devenv/state` are carried only while no local process holds them open.";
      };

      maxFileMb = mkOption {
        type = types.ints.positive;
        default = 64;
        description = "Untracked files above this many MB are skipped and listed.";
      };

      autoIncludeMb = mkOption {
        type = types.ints.unsigned;
        default = 500;
        description = "A new directory at unit depth under this many MB is included by itself; a bigger one waits for a tap in the panel.";
      };
    };

    launch = {
      direnv = mkOption {
        type = types.bool;
        default = config.programs.direnv.enable;
        defaultText = literalExpression "config.programs.direnv.enable";
        description = ''
          Whether sessions started from the panel get the direnv environment
          of the directory they start in (the variables matching
          {option}`launch.env`), through their settings file. `claude --bg`
          runs in Claude Code's background daemon, which never sees the
          collector's environment, so this is the only way in. A direnv
          failure (not allowed, an error) fails the launch.
        '';
      };

      env = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "CLAUDE_META_*" ];
        description = ''
          Globs (`*`, `?`) of the direnv variables launched sessions get;
          nothing passes without a match.
        '';
      };
    };

    dms.enable = mkEnableOption ''
      the DankMaterialShell plugin: bar pill, popout list, dash tab and
      card, notifications, and `dms ipc call ccdash …`. Linked into
      {file}`$XDG_CONFIG_HOME/DankMaterialShell/plugins/ccdash`; enable it
      once in DMS Settings → Plugins and add the widget to a bar
    '';

    systemdTarget = mkOption {
      type = types.str;
      default = "graphical-session.target";
      description = "systemd user target the collector starts with.";
    };

    claudeCode = {
      hooks.enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Whether to register the ccdash hooks in
          {option}`programs.claude-code.settings`.
        '';
      };

      asks.enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Whether ccdash can answer prompts: the PermissionRequest hook
          becomes `ccdash ask`, which holds each tool approval, question
          and plan open so the web UI or a notification button can answer
          it while the terminal dialog stays open. Otherwise the hook only
          reports the prompt. Needs {option}`hooks.enable`.
        '';
      };

      channel = {
        enable = mkEnableOption ''
          replying to sessions from ccdash, through a Claude Code channel.
          Puts a `claude` wrapper first in `PATH` that starts interactive
          sessions with `--channels plugin:ccdash@inline --plugin-dir …`; subcommands,
          `-p` and `--bg` run unchanged (sessions started from the panel get
          the plugin through `[launch] command`). Claude Code only loads the channel
          when it is allowlisted (the NixOS module's
          {option}`programs.ccdash.channel.allow`) or with
          {option}`devFlag`
        '';

        devFlag = mkOption {
          type = types.bool;
          default = false;
          description = ''
            Whether to load the channel with
            `--dangerously-load-development-channels` instead of `--channels`.
            Needs no allowlist, but Claude Code then shows a warning dialog
            at every launch.
          '';
        };
      };

      telemetry = {
        enable = mkEnableOption ''
          Claude Code's OpenTelemetry export to ccdash, for tokens, cache
          hits, API latency, commits, pull requests and edit decisions in
          the analytics view. Sets the `OTEL_*` variables in
          {option}`programs.claude-code.settings.env` and a loopback-only
          receiver in the collector, which forwards the kept records to
          the hub; prompt text stays redacted
        '';

        port = mkOption {
          type = types.port;
          default = 7839;
          description = "Loopback port of the OTLP/HTTP receiver.";
        };
      };

      statusLine = {
        enable = mkEnableOption ''
          the statusline wrapper that feeds model, context, cost and
          rate-limit stats to ccdash. Replaces
          {option}`programs.claude-code.settings.statusLine`
        '';

        command = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = literalExpression "lib.getExe pkgs.ccstatusline";
          description = ''
            Statusline command to run behind the wrapper. `null` collects the
            stats without showing a statusline.
          '';
        };
      };
    };
  };

  config = mkIf cfg.enable (mkMerge [
    {
      home.packages = [ cfg.package ];

      services.ccdash.settings = {
        hub = {
          url = mkDefault cfg.hub.url;
          token_file = mkDefault cfg.hub.tokenFile;
        };
        sync.enable = mkDefault cfg.sync.enable;
        workspace = {
          enable = mkDefault cfg.workspace.enable;
          root = mkDefault cfg.workspace.root;
          depth = mkDefault cfg.workspace.depth;
          include = mkDefault cfg.workspace.include;
          exclude = mkDefault cfg.workspace.exclude;
          always_exclude = mkDefault cfg.workspace.alwaysExclude;
          sync_ignored = mkDefault cfg.workspace.syncIgnored;
          max_file_mb = mkDefault cfg.workspace.maxFileMb;
          auto_include_mb = mkDefault cfg.workspace.autoIncludeMb;
        };
        launch = {
          direnv = mkDefault (
            optionals cfg.launch.direnv [
              (getExe config.programs.direnv.package)
              "export"
              "json"
            ]
          );
          env = mkDefault cfg.launch.env;
        };
      };

      xdg.configFile."ccdash/config.toml".source = configFile;

      systemd.user.services.ccdash = {
        Unit = {
          Description = "ccdash collector, reports Claude Code sessions to the hub";
          PartOf = [ cfg.systemdTarget ];
          After = [ cfg.systemdTarget ];
          X-Restart-Triggers = [ "${configFile}" ];
        };
        Service = {
          ExecStart = "${exe} collector";
          Restart = "on-failure";
          RestartSec = 2;
          # NOTE: the workspace sync spawns git; measured 2026-10-30 under these
          # limits at < 1 % of a core during real work, a seed at ~40 %
          Environment = mkIf cfg.workspace.enable [ "PATH=${lib.makeBinPath [ pkgs.git ]}" ];
        }
        // optionalAttrs cfg.workspace.enable {
          CPUWeight = 20;
          IOWeight = 20;
          Nice = 10;
        };
        Install.WantedBy = [ cfg.systemdTarget ];
      };
    }
    (mkIf (claudeCode.enable && claudeCode.package != null) {
      services.ccdash.settings = {
        agents.command = mkDefault [
          (getExe claudeCode.package)
          "agents"
          "--json"
        ];
        # NOTE: the real binary, not the channel wrapper; ccdash adds `-p /usage`
        usage.command = mkDefault [ (getExe claudeCode.package) ];
        # NOTE: the wrapper passes `--bg` through without the plugin; ccdash adds `--channels`
        launch = {
          command = mkDefault (
            [ (getExe claudeCode.package) ]
            ++ optionals channel.enable [
              "--plugin-dir"
              pluginDir
            ]
          );
          stop = mkDefault [
            (getExe claudeCode.package)
            "stop"
            "{id}"
          ];
          remove = mkDefault [
            (getExe claudeCode.package)
            "rm"
            "{id}"
          ];
        };
      };
    })
    (mkIf cfg.claudeCode.hooks.enable {
      programs.claude-code.settings.hooks = genAttrs hookEvents (event: [
        (
          {
            hooks = [
              (if event == "PermissionRequest" && cfg.claudeCode.asks.enable then askHook else hook)
            ];
          }
          // optionalAttrs (elem event toolEvents) { matcher = "*"; }
        )
      ]);
    })
    (mkIf channel.enable {
      assertions = [
        {
          assertion = claudeCode.enable && claudeCode.package != null;
          message = "services.ccdash.claudeCode.channel needs programs.claude-code.package to wrap.";
        }
      ];
      home.packages = [ (hiPrio claudeWrapper) ];
    })
    (mkIf cfg.claudeCode.telemetry.enable (
      let
        endpoint = "127.0.0.1:${toString cfg.claudeCode.telemetry.port}";
      in
      {
        services.ccdash.settings.telemetry.listen = [ endpoint ];
        programs.claude-code.settings.env = {
          CLAUDE_CODE_ENABLE_TELEMETRY = "1";
          OTEL_METRICS_EXPORTER = "otlp";
          OTEL_LOGS_EXPORTER = "otlp";
          OTEL_EXPORTER_OTLP_PROTOCOL = "http/json";
          OTEL_EXPORTER_OTLP_ENDPOINT = "http://${endpoint}";
          OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE = "delta";
        };
      }
    ))
    (mkIf cfg.dms.enable {
      xdg.configFile."DankMaterialShell/plugins/ccdash".source = "${cfg.package}/share/ccdash/dms-plugin";
    })
    (mkIf cfg.claudeCode.statusLine.enable {
      programs.claude-code.settings.statusLine = mkForce {
        type = "command";
        command = "${exe} tee" + optionalString (statusLineCommand != null) " -- ${statusLineCommand}";
        padding = 0;
      };
    })
  ]);
}
