# SPDX-License-Identifier: AGPL-3.0-or-later
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    optional
    optionalAttrs
    optionalString
    types
    ;

  cfg = config.services.looklab;
  hostPackages = self.packages.${pkgs.stdenv.hostPlatform.system};

  configFile = (pkgs.formats.toml { }).generate "looklab.toml" {
    bind = cfg.bind;
    db_path = "${cfg.stateDir}/looklab.sqlite3";
    blob_dir = "${cfg.stateDir}/blobs";
    server_secret_path = "${cfg.stateDir}/server-secret";
    bodies_dir = "${cfg.dataDir}/bodies";
    manuals_dir = cfg.manualsDir;
    snapshot_dir = "${cfg.stateDir}/community";
    inherit (cfg) origin;
    rp_id = cfg.rpId;
    cors_origins = cfg.corsOrigins;
    site = optionalAttrs (cfg.site.distDir != null) {
      dist_dir = cfg.site.distDir;
    } // optionalAttrs (cfg.site.dataDir != null) {
      data_dir = cfg.site.dataDir;
    };
  };

  litestreamConfig = (pkgs.formats.yaml { }).generate "litestream.yml" {
    dbs = [
      {
        path = "${cfg.stateDir}/looklab.sqlite3";
        replica = {
          inherit (cfg.litestream) url;
          inherit (cfg.litestream) region;
          sync-interval = cfg.litestream.syncInterval;
        } // optionalAttrs (cfg.litestream.endpoint != null) {
          inherit (cfg.litestream) endpoint;
        };
      }
    ];
    snapshot = {
      interval = cfg.litestream.snapshotInterval;
      retention = cfg.litestream.snapshotRetention;
    };
    l0-retention = cfg.litestream.l0Retention;
    socket = {
      enabled = true;
      path = "${cfg.stateDir}/litestream.sock";
    };
  };

  hardening = {
    NoNewPrivileges = true;
    PrivateTmp = true;
    ProtectHome = true;
    ProtectSystem = "strict";
    ReadWritePaths = [ cfg.stateDir ];
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectControlGroups = true;
    RestrictAddressFamilies = [
      "AF_INET"
      "AF_INET6"
      "AF_UNIX"
    ];
    RestrictNamespaces = true;
    LockPersonality = true;
    ProtectClock = true;
    ProtectHostname = true;
    ProtectProc = "invisible";
    ProcSubset = "pid";
    SystemCallArchitectures = "native";
    StateDirectory = cfg.stateDir;
    StateDirectoryMode = "0700";
    UMask = "0077";
  };

  workerArgs = [
    "--db"
    "${cfg.stateDir}/looklab.sqlite3"
    "--blobs"
    "${cfg.stateDir}/blobs"
    "--data"
    cfg.dataDir
    "--manuals"
    cfg.manualsDir
    "--snapshot-dir"
    "${cfg.stateDir}/community"
    "--transforms"
    "${cfg.dataDir}/transforms"
    "--site"
    "${cfg.stateDir}/site"
    "--lease-seconds"
    (toString cfg.worker.leaseSeconds)
    "--seed"
    (toString cfg.worker.seed)
  ];
in
{
  options.services.looklab = {
    enable = mkEnableOption "the looklab server and worker";

    hostname = mkOption {
      type = types.str;
      example = "looklab.example";
      description = "Public hostname, used by Caddy and as the default origin and RP ID.";
    };

    origin = mkOption {
      type = types.str;
      default = "https://${cfg.hostname}";
      description = "Exact browser origin pinned for every passkey credential.";
    };

    rpId = mkOption {
      type = types.str;
      default = cfg.hostname;
      description = "WebAuthn relying-party ID: the origin's host or a parent domain.";
    };

    corsOrigins = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Extra browser origins allowed to call the API; empty for a single origin.";
    };

    bind = mkOption {
      type = types.str;
      default = "127.0.0.1:8080";
      description = "Server listener address. Keep it on loopback behind Caddy.";
    };

    stateDir = mkOption {
      type = types.str;
      default = "/var/lib/looklab";
      description = "Private state root: database, blobs, secret and publication tree.";
    };

    dataDir = mkOption {
      type = types.str;
      default = "${cfg.stateDir}/data";
      description = "Body, transform and scene tree the worker reads.";
    };

    manualsDir = mkOption {
      type = types.str;
      default = "${cfg.dataDir}/manuals";
      description = "Documentation-derived camera catalog the server and worker read.";
    };

    scenesDir = mkOption {
      type = types.nullOr types.path;
      default = hostPackages.looklab-scenes;
      defaultText = lib.literalExpression "self.packages.<system>.looklab-scenes";
      description = "Prepared photo scenes (PNGs plus index.json) copied under dataDir/scenes on first start.";
    };

    site.distDir = mkOption {
      type = types.nullOr types.path;
      default = "${hostPackages.looklab-site}/dist";
      description = "Built SPA served at /. Null runs the API without a page.";
    };

    site.dataDir = mkOption {
      type = types.nullOr types.str;
      default = "${cfg.stateDir}/site/data";
      description = "Directory the worker publishes into and the server serves at /data. Null withholds /data.";
    };

    user = mkOption {
      type = types.str;
      default = "looklab";
      description = "System user running the server and both worker units.";
    };

    group = mkOption {
      type = types.str;
      default = cfg.user;
      description = "System group running the server and both worker units.";
    };

    server.package = mkOption {
      type = types.package;
      default = hostPackages.looklab-server;
      defaultText = lib.literalExpression "self.packages.<system>.looklab-server";
    };

    cli.package = mkOption {
      type = types.package;
      default = hostPackages.looklab-cli;
      defaultText = lib.literalExpression "self.packages.<system>.looklab-cli";
      description = "Operator CLI installed as `looklab` for the offline grant and revoke path.";
    };

    worker = {
      package = mkOption {
        type = types.package;
        default = hostPackages.looklab-worker;
        defaultText = lib.literalExpression "self.packages.<system>.looklab-worker";
      };

      leaseSeconds = mkOption {
        type = types.ints.positive;
        default = 300;
        description = "Job lease; a restarted worker reclaims an interrupted job after at most this long.";
      };

      seed = mkOption {
        type = types.ints.unsigned;
        default = 1592597086;
        description = "Fit sampling seed for the worker's own checks; the binary's default is 0x5eed1a5e.";
      };

      memoryMax = mkOption {
        type = types.str;
        default = "5G";
        description = "cgroup memory ceiling for the worker, which dominates peak use during refits.";
      };

      memoryHigh = mkOption {
        type = types.str;
        default = "4G";
        description = "cgroup memory throttle for the worker.";
      };
    };

    publisher = {
      memoryMax = mkOption {
        type = types.str;
        default = "512M";
        description = "cgroup memory ceiling for the publication worker.";
      };

      memoryHigh = mkOption {
        type = types.str;
        default = "384M";
        description = "cgroup memory throttle for the publication worker.";
      };
    };

    caddy = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Terminate TLS in Caddy and forward to the loopback listener.";
      };

      acmeEmail = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "ACME account email Caddy registers with the certificate authority.";
      };
    };

    litestream = {
      enable = mkOption {
        type = types.bool;
        default = false;
        description = "Replicate the database to an S3-compatible replica.";
      };

      url = mkOption {
        type = types.str;
        default = "";
        example = "s3://looklab-sqlite/looklab";
        description = "Replica URL.";
      };

      region = mkOption {
        type = types.str;
        default = "";
        example = "eu-central-1";
        description = "Replica region.";
      };

      endpoint = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "S3-compatible endpoint for non-AWS replicas.";
      };

      envFile = mkOption {
        type = types.str;
        default = "/var/lib/looklab-secrets/litestream.env";
        description = "Root-only EnvironmentFile holding the replica credentials.";
      };

      syncInterval = mkOption {
        type = types.str;
        default = "1s";
      };

      snapshotInterval = mkOption {
        type = types.str;
        default = "24h";
      };

      snapshotRetention = mkOption {
        type = types.str;
        default = "720h";
      };

      l0Retention = mkOption {
        type = types.str;
        default = "24h";
      };
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.site.distDir != null || cfg.corsOrigins != [ ];
        message = "services.looklab: an API without a served SPA needs corsOrigins for the page origin.";
      }
      {
        assertion = !cfg.litestream.enable || (cfg.litestream.url != "" && cfg.litestream.region != "");
        message = "services.looklab.litestream: url and region are required when replication is enabled.";
      }
    ];

    users.groups.${cfg.group} = { };
    users.users.${cfg.user} = {
      isSystemUser = true;
      inherit (cfg) group;
      home = cfg.stateDir;
      homeMode = "0700";
      description = "looklab server and worker";
    };

    environment.systemPackages = [
      cfg.cli.package
      cfg.server.package
      pkgs.curl
      pkgs.sqlite
      pkgs.litestream
    ];

    environment.etc = {
      "looklab/looklab.toml".source = configFile;
      "looklab/looklab.toml".mode = "0644";
    }
    // optionalAttrs cfg.litestream.enable {
      "looklab/litestream.yml".source = litestreamConfig;
      "looklab/litestream.yml".mode = "0644";
    };

    systemd.tmpfiles.rules =
      [
        "d ${cfg.stateDir} 0700 ${cfg.user} ${cfg.group} -"
        "d ${cfg.dataDir} 0700 ${cfg.user} ${cfg.group} -"
        "d ${cfg.manualsDir} 0700 ${cfg.user} ${cfg.group} -"
      ]
      ++ optional cfg.litestream.enable "d ${dirOf cfg.litestream.envFile} 0700 root root -";

    systemd.services.looklab-seed-data = {
      description = "Seed the looklab data directory";
      wantedBy = [ "multi-user.target" ];
      before = [
        "looklab-server.service"
        "looklab-worker.service"
        "looklab-publisher.service"
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        User = cfg.user;
        Group = cfg.group;
        UMask = "0077";
        StateDirectory = cfg.stateDir;
        StateDirectoryMode = "0700";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectHome = true;
        ProtectSystem = "strict";
        ReadWritePaths = [ cfg.stateDir cfg.dataDir cfg.manualsDir ];
      };
      script = ''
        set -eu
        if [ ! -e ${cfg.dataDir}/.seeded ]; then
          mkdir -p ${cfg.dataDir}
          cp -R ${hostPackages.looklab-data}/. ${cfg.dataDir}/
          touch ${cfg.dataDir}/.seeded
        fi
        if [ -d ${hostPackages.looklab-manuals} ]; then
          mkdir -p "${cfg.manualsDir}"
          cp -R --remove-destination "${hostPackages.looklab-manuals}/." "${cfg.manualsDir}/"
        fi
        ${
          optionalString (cfg.scenesDir != null) ''
            if [ ! -e ${cfg.dataDir}/scenes/index.json ]; then
              mkdir -p ${cfg.dataDir}/scenes
              cp -R ${cfg.scenesDir}/. ${cfg.dataDir}/scenes/
            fi
          ''
        }
      '';
    };

    systemd.services.looklab-server = {
      description = "looklab API and site server";
      wantedBy = [ "multi-user.target" ];
      restartTriggers = [ hostPackages.looklab-data ];
      after = [
        "network.target"
        "looklab-seed-data.service"
      ];
      requires = [ "looklab-seed-data.service" ];
      serviceConfig = hardening // {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStartPre = "${cfg.server.package}/bin/looklab-server init --config ${configFile}";
        ExecStart = "${cfg.server.package}/bin/looklab-server --config ${configFile}";
        Restart = "on-failure";
        RestartSec = 2;
        KillSignal = "SIGTERM";
        TimeoutStopSec = 60;
        MemoryDenyWriteExecute = true;
      };
    };

    systemd.services.looklab-worker = {
      description = "looklab fit and job runner";
      wantedBy = [ "multi-user.target" ];
      restartTriggers = [ hostPackages.looklab-data ];
      after = [
        "network.target"
        "looklab-server.service"
        "looklab-seed-data.service"
      ];
      requires = [ "looklab-seed-data.service" ];
      serviceConfig = hardening // {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = lib.escapeShellArgs (
          [ "${cfg.worker.package}/bin/looklab-worker" ] ++ workerArgs ++ [ "--lane" "fit" ]
        );
        Restart = "on-failure";
        RestartSec = 5;
        KillSignal = "SIGTERM";
        TimeoutStopSec = 600;
        MemoryMax = cfg.worker.memoryMax;
        MemoryHigh = cfg.worker.memoryHigh;
      };
    };

    systemd.services.looklab-publisher = {
      description = "looklab publication worker";
      wantedBy = [ "multi-user.target" ];
      restartTriggers = [ hostPackages.looklab-data ];
      after = [
        "network.target"
        "looklab-server.service"
        "looklab-seed-data.service"
      ];
      requires = [ "looklab-seed-data.service" ];
      serviceConfig = hardening // {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = lib.escapeShellArgs (
          [ "${cfg.worker.package}/bin/looklab-worker" ] ++ workerArgs ++ [ "--lane" "publish" ]
        );
        Restart = "on-failure";
        RestartSec = 5;
        KillSignal = "SIGTERM";
        TimeoutStopSec = 600;
        MemoryMax = cfg.publisher.memoryMax;
        MemoryHigh = cfg.publisher.memoryHigh;
      };
    };

    services.caddy = mkIf cfg.caddy.enable {
      enable = true;
      email = cfg.caddy.acmeEmail;
      virtualHosts.${cfg.hostname}.extraConfig = ''
        reverse_proxy ${cfg.bind}
      '';
    };

    systemd.services.looklab-litestream = mkIf cfg.litestream.enable {
      description = "Litestream replication for the looklab database";
      wantedBy = [ "multi-user.target" ];
      after = [ "looklab-server.service" ];
      requires = [ "looklab-server.service" ];
      serviceConfig = hardening // {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        EnvironmentFile = cfg.litestream.envFile;
        ExecStart = "${pkgs.litestream}/bin/litestream replicate -config ${litestreamConfig}";
        Restart = "always";
        RestartSec = 5;
        TimeoutStopSec = 60;
      };
    };


    networking.firewall.enable = true;
    networking.firewall.allowedTCPPorts = [
      80
      443
    ];
  };
}
