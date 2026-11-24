# --- flake-parts/modules/home-manager/ccdash.nix
{ localFlake }:
{
  config,
  lib,
  options,
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
  # NOTE: sessions run with the `PATH` of whoever dispatched them, here the collector, so it
  # keeps the user manager's: `Environment=PATH=` would replace it
  collector = pkgs.writeShellScript "ccdash-collector" ''
    PATH=${lib.makeBinPath [ pkgs.git ]}:$PATH
    exec ${exe} collector
  '';
  statusLineCommand = cfg.claudeCode.statusLine.command;

  # NOTE: src/win/setup/settings.rs keeps a copy (EVENTS, TOOL_EVENTS)
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
  # NOTE: `programs.claude-code.plugins` arrived in home-manager late in 2026
  canPlugin = options.programs ? claude-code && options.programs.claude-code ? plugins;
  claudeExe =
    if claudeCode.enable && claudeCode.package != null then getExe claudeCode.package else "claude";
  inherit (cfg) terminal;
  # NOTE: `claude <subcommand>` parsers reject the channel flags (`rc` included);
  # windows/claude-channel.ps1 keeps a copy of the list
  claudeWrapper = pkgs.writeShellScriptBin "claude" ''
    real=${getExe claudeCode.package}
    case "''${1-}" in
      agents | attach | auth | auto-mode | daemon | doctor | gateway | import | install | logs | \
        mcp | plugin | plugins | purge | rc | remote-control | respawn | rm | setup-token | stop | \
        kill | ultrareview | update | upgrade)
        exec "$real" "$@"
        ;;
    esac
    # NOTE: ccdash's flag, not Claude Code's: the session's hooks hand it to the hub
    project=
    args=()
    while [ $# -gt 0 ]; do
      case $1 in
        --)
          args+=("$@")
          break
          ;;
        --project=*)
          project=''${1#--project=}
          shift
          ;;
        --project)
          if [ $# -lt 2 ]; then
            echo "claude: --project needs a project name" >&2
            exit 2
          fi
          project=$2
          shift 2
          ;;
        *)
          args+=("$1")
          shift
          ;;
      esac
    done
    set -- "''${args[@]}"
    if [ -n "$project" ]; then
      export CCDASH_PROJECT=$project
    fi
    for arg in "$@"; do
      case $arg in
        --) break ;;
        --bg | --background)
          if [ -n "$project" ]; then
            echo "claude: --project cannot reach a --bg session: start it from the panel" >&2
            exit 2
          fi
          exec "$real" "$@"
          ;;
        -p | --print) exec "$real" "$@" ;;
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
      local = mkEnableOption ''
        a hub on this machine (the NixOS module with
        `services.ccdash-hub.localCollector` for this user): {option}`url`
        and {option}`tokenFile` default to it
      '';

      url = mkOption {
        type = types.str;
        example = "wss://ccdash.example.org/collect";
        description = ''
          The hub's `/collect` endpoint; with {option}`local`
          `ws://127.0.0.1:7837/collect` (set it for another port).
        '';
      };

      tokenFile = mkOption {
        type = types.str;
        example = "\${XDG_RUNTIME_DIR}/agenix/ccdash-collector-token";
        description = ''
          File holding this machine's collector token, the one the hub
          lists under this host name. `$VAR` and `''${VAR}` expand. With
          {option}`local` the token the hub generated for this machine.
        '';
      };
    };

    terminal = mkOption {
      type = types.nullOr (
        types.submodule {
          options = {
            command = mkOption {
              type = types.listOf types.str;
              example = [
                "ghostty"
                "-e"
              ];
              description = "Opens a new terminal window running the program and arguments that follow.";
            };
            appId = mkOption {
              type = types.nullOr types.str;
              default = null;
              example = "com.mitchellh.ghostty";
              description = ''
                The terminal's window app id, so ccdash ties a session to its
                window on niri (focus, `ccdash open`). `null` keeps the
                built-in list of common terminals.
              '';
            };
          };
        }
      );
      default = null;
      example = literalExpression ''
        {
          command = [ "ghostty" "-e" ];
          appId = "com.mitchellh.ghostty";
        }
      '';
      description = ''
        The terminal ccdash opens things in on this machine: background
        sessions (the panel's Open, `ccdash open`) and, with {option}`editor`,
        drafts. `null` turns both off; the panel then hides them.
      '';
    };

    editor = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "nvim";
      description = ''
        The editor the panel's Edit in the editor (Ctrl G) opens a draft in,
        inside {option}`terminal`. It must return when the file is closed;
        exit status 0 takes the text, anything else (`:cq`) cancels. `null`
        turns it off; an editor with a window of its own goes in
        {option}`settings.edit.command`.
      '';
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

    transcribe = {
      enable = mkEnableOption ''
        putting recordings into words on this machine with whisper.cpp: the
        panel's dictation (the microphone in a message field), and what is
        said in a recording or a video attached to a prompt, which Claude
        cannot hear. Everything runs here; nothing leaves the machine. Sets
        {option}`settings.transcribe.command`
      '';

      package = mkOption {
        type = types.package;
        default = pkgs.whisper-cpp;
        defaultText = literalExpression "pkgs.whisper-cpp";
        description = "The whisper.cpp package; its `whisper-cli` is run.";
      };

      model = mkOption {
        type = types.path;
        default = pkgs.fetchurl {
          url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin";
          hash = "sha256-G+OpsgY4Z7k35k4ux0gzZKeZF+FX+pjF2UtcH//qmHs=";
        };
        defaultText = literalExpression ''
          pkgs.fetchurl {
            url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin";
            hash = "sha256-G+OpsgY4Z7k35k4ux0gzZKeZF+FX+pjF2UtcH//qmHs=";
          }
        '';
        description = ''
          The model file (ggml). The default is `small`, 466 MB, many
          languages: a sentence takes a few seconds on a desktop's
          processor. `base` (142 MB) is faster and worse outside English;
          `medium` and `large-v3-turbo` (1.5 GB each) are better and want a
          strong machine. Fetch another the same way, with its own hash.
        '';
      };

      language = mkOption {
        type = types.str;
        default = "auto";
        example = "cs";
        description = ''
          The language spoken, as whisper names it (`en`, `cs`, ...); `auto`
          lets it tell from the first seconds, which goes wrong more often
          on a short sentence.
        '';
      };

      extraArgs = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [
          "--threads"
          "8"
        ];
        description = "More arguments for `whisper-cli`, before the file.";
      };
    };

    video = {
      enable = mkEnableOption ''
        cutting still frames from videos attached to a prompt, which Claude
        cannot watch: the prompt then names the frames (and, with
        {option}`transcribe.enable`, what is said). Sets
        {option}`settings.video.ffmpeg`; the number of frames and their
        size are {option}`settings.video.frames` (12) and
        {option}`settings.video.edge` (1568)
      '';

      package = mkOption {
        type = types.package;
        default = pkgs.ffmpeg-headless;
        defaultText = literalExpression "pkgs.ffmpeg-headless";
        description = "The ffmpeg package; its `ffmpeg` and `ffprobe` are run.";
      };
    };

    power.lock = mkEnableOption ''
      locking this machine's graphical session from the panel (Machines ›
      the machine › Lock, also on its card while the session is unlocked).
      The collector runs {option}`settings.power.lock_command`
      (`loginctl lock-session` for the user's display session), which any
      locker listening to logind answers, DankMaterialShell's included
    '';

    power.suspend = mkEnableOption ''
      suspending this machine from the panel (Machines › the machine ›
      Suspend). The collector hands the hub what is pending of Claude Code's
      state, then runs {option}`settings.power.suspend_command`
      (`systemctl suspend`), which logind allows the user of an active seat
      session. Waking it again needs wake-on-LAN on a wired card and a hub
      that reaches its network (see the hub's `wake.via`)
    '';

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
        type = types.nullOr types.str;
        default = null;
        example = "~/code";
        description = ''
          The workspace root; `~/` and `$VAR` expand. The hub's relative
          workspaces (`work`, `clients/acme` in its `workspaces`) are
          directories under it on this machine, with the sync or without;
          {option}`enable` needs it.
        '';
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

      daemon.enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Whether Claude Code's background supervisor runs as its own user
          service, instead of being started by the first `claude --bg`.
          Started from the panel it would live in the collector's cgroup
          and die with every collector restart, taking the background
          sessions with it, and it would keep the environment of whatever
          started it. Restarting this service keeps the sessions: the next
          supervisor adopts them. It starts with {file}`default.target`, so
          a host nobody logs in to needs `loginctl enable-linger`. Needs
          {option}`programs.claude-code.package`.
        '';
      };

      attachments.enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Whether sessions may read the files attached to prompts in the
          panel without asking: adds a `Read` rule for the collector's
          attachments directory (`attachments/` in ccdash's state
          directory) to {option}`programs.claude-code.settings.permissions.allow`.
          Without it Claude asks before opening each attached file.
          Sessions the panel starts get the rule through their own
          settings either way. Set {option}`settings.attachments.enable`
          to false to stop this machine fetching attachments at all.
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

      plugin.enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Whether every Claude Code session loads the ccdash plugin: the
          knowledge tools (`recall`, `remember`, `issue_add`, …) and the
          `ccdash:wrap-up`, `ccdash:report` and `ccdash:project` skills.
          Installed through {option}`programs.claude-code.plugins` (Claude
          Code 2.1.157 or later); sessions with the channel load the same
          plugin through `--plugin-dir`, which takes precedence.
        '';
      };

      channel = {
        enable = mkEnableOption ''
          replying to sessions from ccdash, through a Claude Code channel.
          Puts a `claude` wrapper first in `PATH` that starts interactive
          sessions with `--channels plugin:ccdash@inline --plugin-dir …`; subcommands,
          `-p` and `--bg` run unchanged (sessions started from the panel get
          the plugin through `[launch] command`). The wrapper also takes
          `--project <name>`, which starts the session under that project of
          the workspace. Claude Code only loads the channel
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
      assertions = [
        {
          assertion = !cfg.workspace.enable || cfg.workspace.root != null;
          message = "services.ccdash.workspace.enable needs services.ccdash.workspace.root.";
        }
      ];

      home.packages = [ cfg.package ];

      services.ccdash.settings = {
        hub = {
          url = mkDefault cfg.hub.url;
          token_file = mkDefault cfg.hub.tokenFile;
        };
        sync.enable = mkDefault cfg.sync.enable;
        power = {
          suspend = mkDefault cfg.power.suspend;
          lock = mkDefault cfg.power.lock;
        };
        workspace = {
          enable = mkDefault cfg.workspace.enable;
          root = mkIf (cfg.workspace.root != null) (mkDefault cfg.workspace.root);
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
          channel = mkDefault channel.enable;
        };
        attach.command = mkIf (terminal != null) (
          mkDefault (
            terminal.command
            ++ [
              claudeExe
              "attach"
              "{id}"
            ]
          )
        );
        transcribe.command = mkIf cfg.transcribe.enable (
          mkDefault (
            [
              "${cfg.transcribe.package}/bin/whisper-cli"
              "-m"
              "${cfg.transcribe.model}"
              "-l"
              cfg.transcribe.language
              # NOTE: the text alone on stdout: no timestamps, no progress
              "-nt"
              "-np"
            ]
            ++ cfg.transcribe.extraArgs
            ++ [
              "-f"
              "{file}"
            ]
          )
        );
        video.ffmpeg = mkIf cfg.video.enable (mkDefault "${cfg.video.package}/bin/ffmpeg");
        edit.command = mkIf (terminal != null && cfg.editor != null) (
          mkDefault (
            terminal.command
            ++ [
              cfg.editor
              "{file}"
            ]
          )
        );
        niri.terminals = mkIf (terminal != null && terminal.appId != null) (mkDefault [ terminal.appId ]);
      };

      services.ccdash.hub = mkIf cfg.hub.local {
        url = mkDefault "ws://127.0.0.1:7837/collect";
        # NOTE: the NixOS hub module's `localCollector` generates it there
        tokenFile = mkDefault "/var/lib/ccdash-hub-local/collector.token";
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
          # NOTE: the workspace sync spawns git; measured 2026-10-30 under these
          # limits at < 1 % of a core during real work, a seed at ~40 %
          ExecStart = if cfg.workspace.enable then "${collector}" else "${exe} collector";
          Restart = "on-failure";
          RestartSec = 2;
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
          "--all"
        ];
        # NOTE: the real binary, not the channel wrapper; ccdash adds `-p /usage`
        usage.command = mkDefault [ (getExe claudeCode.package) ];
        # NOTE: the composer's `/` menu reads a skills-dir plugin by itself
        skills.plugin_dirs = mkDefault (
          optionals (channel.enable && !(cfg.claudeCode.plugin.enable && canPlugin)) [ pluginDir ]
        );
        # NOTE: the wrapper passes `--bg` through without the plugin; ccdash adds `--channels`
        # (`launch.channel`)
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

      systemd.user.services.claude-daemon = mkIf cfg.claudeCode.daemon.enable {
        Unit = {
          Description = "Claude Code's supervisor of background sessions";
          # NOTE: exits 1 while a supervisor started on demand still runs: wait it out
          StartLimitIntervalSec = 0;
        };
        Service = {
          ExecStart = "${getExe claudeCode.package} daemon run";
          # NOTE: the supervisor only, the next one adopts the running sessions; the
          # default kills them with it (both verified on 2.1.312)
          KillMode = "process";
          Restart = "always";
          RestartSec = 5;
        };
        Install.WantedBy = [ "default.target" ];
      };
    })
    (mkIf (cfg.claudeCode.attachments.enable && (cfg.settings.attachments.enable or true)) {
      # NOTE: `//` starts an absolute path in Claude Code's permission rules
      programs.claude-code.settings.permissions.allow = [
        "Read(/${cfg.settings.attachments.dir or "${config.xdg.stateHome}/ccdash/attachments"}/**)"
      ];
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
    (mkIf (cfg.claudeCode.plugin.enable && claudeCode.enable) (
      if canPlugin then
        { programs.claude-code.plugins.ccdash = pluginDir; }
      else
        {
          warnings = [
            "services.ccdash.claudeCode.plugin: this home-manager has no programs.claude-code.plugins, so only sessions with the channel get the ccdash tools and skills."
          ];
        }
    ))
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
