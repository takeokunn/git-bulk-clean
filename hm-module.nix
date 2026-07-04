{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.git-maintenance;
  boolToStr = b: if b then "true" else "false";

  pkg = self.packages.${pkgs.system}.default;
  isDarwin = pkgs.stdenv.hostPlatform.isDarwin;

  # Single source of truth for the daemon's environment. Rendered as an attrset
  # for launchd and as a "KEY=value" list for systemd.
  envAttrs = {
    MAINTENANCE_REPOS = lib.concatStringsSep "," cfg.repositories;
    MAINTENANCE_GHQ_ENABLE = boolToStr cfg.ghq.enable;
    MAINTENANCE_REFLOG_EXPIRE = cfg.reflogExpire;
    MAINTENANCE_AGGRESSIVE = boolToStr cfg.aggressive;
    MAINTENANCE_INTERVAL = toString cfg.interval;
    MAINTENANCE_WORKERS = toString cfg.workers;
    MAINTENANCE_SKIP_SUBMODULES = boolToStr cfg.skipSubmodules;
    MAINTENANCE_SKIP_LFS = boolToStr cfg.skipLfs;
    MAINTENANCE_PRUNE_BRANCHES = boolToStr cfg.pruneBranches;
    MAINTENANCE_PROTECTED_BRANCHES = lib.concatStringsSep "," cfg.protectedBranches;
  };
  envList = lib.mapAttrsToList (name: value: "${name}=${value}") envAttrs;
in
{
  options.services.git-maintenance = {
    enable = lib.mkEnableOption "git-bulk-clean maintenance service";

    ghq.enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Include all repositories managed by ghq";
    };

    repositories = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Additional repository paths to maintain";
    };

    interval = lib.mkOption {
      type = lib.types.ints.positive;
      default = 86400;
      description = ''
        Seconds between maintenance cycles. On Linux this is the daemon's
        internal sleep; on macOS it is the launchd StartInterval that triggers
        a one-shot run.
      '';
    };

    reflogExpire = lib.mkOption {
      type = lib.types.str;
      default = "30.days.ago";
      description = "Reflog expiry cutoff passed to git reflog expire --expire";
    };

    aggressive = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Run git gc --aggressive instead of git gc --auto";
    };

    workers = lib.mkOption {
      type = lib.types.ints.positive;
      default = 5;
      description = "Number of parallel worker threads used to process repositories";
    };

    skipSubmodules = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Skip the submodule sync and gc phase even when .gitmodules is present";
    };

    skipLfs = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Skip git lfs prune even when filter.lfs is configured";
    };

    pruneBranches = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Delete local branches that have been merged into the mainline (non-bare only)";
    };

    protectedBranches = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Branch names to never delete when pruneBranches is enabled (mainline is always protected)";
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    { home.packages = [ pkg ]; }

    # Linux: a long-lived systemd user service running the daemon at idle
    # CPU/IO priority so it never competes with active work.
    (lib.mkIf (!isDarwin) {
      systemd.user.services.git-maintenance = {
        Unit = {
          Description = "git-bulk-clean: parallel Git repository maintenance";
          After = [ "network.target" ];
        };

        Service = {
          ExecStart = "${pkg}/bin/git-bulk-clean --daemon";
          Environment = envList;
          Restart = "always";

          # Run at lowest CPU and I/O priority to avoid impacting system load
          Nice = 19;
          IOSchedulingClass = "idle";

          # Security hardening
          NoNewPrivileges = true;
          PrivateTmp = true;
          ProtectSystem = "strict";
          MemoryDenyWriteExecute = true;
          RestrictNamespaces = true;
          LockPersonality = true;
          RestrictRealtime = true;
          RestrictSUIDSGID = true;
        };

        Install = {
          WantedBy = [ "default.target" ];
        };
      };
    })

    # macOS: launchd has no daemon-sleep concept, so run one-shot on a
    # StartInterval at background priority instead of the internal loop.
    (lib.mkIf isDarwin {
      launchd.agents.git-maintenance = {
        enable = true;
        config = {
          ProgramArguments = [ "${pkg}/bin/git-bulk-clean" ];
          EnvironmentVariables = envAttrs;
          StartInterval = cfg.interval;
          RunAtLoad = true;
          ProcessType = "Background";
          LowPriorityIO = true;
          Nice = 19;
          StandardOutPath = "${config.home.homeDirectory}/Library/Logs/git-maintenance.log";
          StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/git-maintenance.log";
        };
      };
    })
  ]);
}
