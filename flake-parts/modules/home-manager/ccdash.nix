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

  channel = cfg.claudeCode.channel;
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
    } plugin:ccdash@inline --plugin-dir ${cfg.package}/share/ccdash/claude-plugin "$@"
  '';
in
{
  options.services.ccdash = {
    enable = mkEnableOption "ccdash, a live dashboard for running Claude Code sessions";

    package = mkOption {
      type = types.package;
      default = localFlake.packages.${pkgs.stdenv.hostPlatform.system}.ccdash;
      defaultText = literalExpression "ccdash.packages.\${system}.ccdash";
      description = "The ccdash package.";
    };

    settings = mkOption {
      inherit (toml) type;
      default = { };
      example = literalExpression ''
        {
          http.listen = [ "127.0.0.1:7837" "[::]:7837" ];
          niri.terminals = [ "com.mitchellh.ghostty" "foot" ];
        }
      '';
      description = ''
        Daemon configuration, written to
        {file}`$XDG_CONFIG_HOME/ccdash/config.toml`. Unset keys keep their
        built-in defaults.
      '';
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
      description = "systemd user target the daemon starts with.";
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

      channel = {
        enable = mkEnableOption ''
          replying to sessions and answering their permission prompts from
          ccdash, through a Claude Code channel. Puts a `claude` wrapper
          first in `PATH` that starts interactive sessions with
          `--channels plugin:ccdash@inline --plugin-dir …`; subcommands,
          `-p` and `--bg` run unchanged. Claude Code only loads the channel
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
          receiver in the daemon; nothing leaves the machine, and prompt
          text stays redacted
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

      xdg.configFile."ccdash/config.toml" = mkIf (cfg.settings != { }) { source = configFile; };

      systemd.user.services.ccdash = {
        Unit = {
          Description = "ccdash, live dashboard for Claude Code sessions";
          PartOf = [ cfg.systemdTarget ];
          After = [ cfg.systemdTarget ];
          X-Restart-Triggers = mkIf (cfg.settings != { }) [ "${configFile}" ];
        };
        Service = {
          ExecStart = "${exe} daemon";
          Restart = "on-failure";
          RestartSec = 2;
        };
        Install.WantedBy = [ cfg.systemdTarget ];
      };
    }
    (mkIf (claudeCode.enable && claudeCode.package != null) {
      services.ccdash.settings.agents.command = mkDefault [
        (getExe claudeCode.package)
        "agents"
        "--json"
      ];
    })
    (mkIf cfg.claudeCode.hooks.enable {
      programs.claude-code.settings.hooks = genAttrs hookEvents (event: [
        ({ hooks = [ hook ]; } // optionalAttrs (elem event toolEvents) { matcher = "*"; })
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
