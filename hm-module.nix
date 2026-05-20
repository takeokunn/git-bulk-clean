{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.git-maintenance;
  boolToStr = b: if b then "true" else "false";
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
      default = [];
      description = "Additional repository paths to maintain";
    };

    interval = lib.mkOption {
      type = lib.types.int;
      default = 86400;
      description = "Seconds to sleep between maintenance cycles (daemon mode)";
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
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ self.packages.${pkgs.system}.default ];

    systemd.user.services.git-maintenance = {
      Unit = {
        Description = "git-bulk-clean: parallel Git repository maintenance";
        After = [ "network.target" ];
      };

      Service = {
        ExecStart = "${self.packages.${pkgs.system}.default}/bin/git-bulk-clean --daemon";

        Environment = [
          "MAINTENANCE_REPOS=${lib.concatStringsSep "," cfg.repositories}"
          "MAINTENANCE_GHQ_ENABLE=${boolToStr cfg.ghq.enable}"
          "MAINTENANCE_REFLOG_EXPIRE=${cfg.reflogExpire}"
          "MAINTENANCE_AGGRESSIVE=${boolToStr cfg.aggressive}"
          "MAINTENANCE_INTERVAL=${toString cfg.interval}"
        ];

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
  };
}
