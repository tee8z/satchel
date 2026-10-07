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
    types
    ;
  cfg = config.services.satchel;
  toml = pkgs.formats.toml { };
  withoutNulls = lib.filterAttrs (_: value: value != null);
  configFile = toml.generate "satchel.toml" {
    server = withoutNulls {
      bind_address = cfg.listenAddress;
      public_url = cfg.publicUrl;
      database_path = "/var/lib/satchel/wallet.db";
      metrics_address = cfg.metricsAddress;
      client_ip_header = cfg.clientIpHeader;
      admin_password_hash_file = if cfg.adminPasswordHashFile == null then null else "admin-password-hash";
      reserved_usernames = cfg.reservedUsernames;
      session_days = cfg.sessionDays;
      allow_private_lnurl_hosts = cfg.allowPrivateLnurlHosts;
    };
    lnd = withoutNulls {
      rest_host = cfg.lnd.restHost;
      tls_cert_path = "lnd-tls";
      macaroon_path = "lnd-macaroon";
      request_timeout_secs = cfg.lnd.requestTimeoutSecs;
      payment_timeout_secs = cfg.lnd.paymentTimeoutSecs;
      expected_network = cfg.lnd.expectedNetwork;
    };
    limits = cfg.limits;
    faucet = cfg.faucet;
    rate_limits = cfg.rateLimits;
  };
  credentialPath = types.strMatching "/[^\n:]+";
in
{
  options.services.satchel = {
    enable = mkEnableOption "Satchel, a multi-account Lightning wallet for test networks only";
    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "Satchel package to run.";
    };
    listenAddress = mkOption {
      type = types.str;
      default = "127.0.0.1:8095";
      description = "Private HTTP socket address. This module does not open firewall ports.";
    };
    publicUrl = mkOption {
      type = types.str;
      example = "https://wallet.example.org";
      description = "Public HTTPS origin. Lightning Addresses use its host.";
    };
    metricsAddress = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "127.0.0.1:9095";
      description = "Optional private listener for /metrics and /healthz.";
    };
    clientIpHeader = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "x-forwarded-for";
      description = "Header a trusted reverse proxy sets to the client address (right-most entry), for rate limits.";
    };
    adminPasswordHashFile = mkOption {
      type = types.nullOr credentialPath;
      default = null;
      description = "Runtime path to the operator's argon2id hash (satchel hash-password). Null disables /admin.";
    };
    reservedUsernames = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Usernames nobody may register, on top of the built-in list.";
    };
    sessionDays = mkOption {
      type = types.ints.positive;
      default = 14;
      description = "How long a login lasts.";
    };
    allowPrivateLnurlHosts = mkOption {
      type = types.bool;
      default = false;
      description = "Allow paying Lightning Addresses on loopback or private addresses (local regtest only).";
    };
    lnd = {
      restHost = mkOption {
        type = types.str;
        example = "127.0.0.1:8080";
        description = "LND REST socket address. Connections use HTTPS and verify its certificate.";
      };
      tlsCertPath = mkOption {
        type = credentialPath;
        description = "Runtime path to LND's TLS certificate; loaded with systemd credentials.";
      };
      macaroonPath = mkOption {
        type = credentialPath;
        description = ''
          Runtime path to a macaroon with info:read invoices:read invoices:write offchain:read
          offchain:write onchain:read; never copied into the Nix store.
        '';
      };
      expectedNetwork = mkOption {
        type = types.nullOr (
          types.enum [
            "testnet"
            "testnet4"
            "signet"
            "regtest"
            "simnet"
          ]
        );
        default = null;
        example = "signet";
        description = "Refuse to start unless LND reports this network. Mainnet is always refused.";
      };
      requestTimeoutSecs = mkOption {
        type = types.ints.positive;
        default = 10;
        description = "Deadline for an LND request.";
      };
      paymentTimeoutSecs = mkOption {
        type = types.ints.positive;
        default = 60;
        description = "How long LND may try to route a payment.";
      };
    };
    limits = mkOption {
      type = toml.type;
      default = { };
      example = {
        max_balance_sat = 1000000;
        max_payment_sat = 250000;
      };
      description = "The [limits] table; see example/config.toml.example for keys and defaults.";
    };
    faucet = mkOption {
      type = toml.type;
      default = { };
      example = {
        enabled = true;
        amount_sat = 10000;
      };
      description = "The [faucet] table. The faucet is off unless enabled here.";
    };
    rateLimits = mkOption {
      type = toml.type;
      default = { };
      description = "The [rate_limits] table.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = builtins.match "https://[^/@?#]+/?" cfg.publicUrl != null;
        message = "services.satchel.publicUrl must be an HTTPS origin without a path, query, or credentials.";
      }
    ];

    systemd.services.satchel = {
      description = "Satchel (test networks only)";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      # Paying other Lightning Addresses needs the public CA roots.
      environment.SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} --config ${configFile}";
        LoadCredential = [
          "lnd-tls:${cfg.lnd.tlsCertPath}"
          "lnd-macaroon:${cfg.lnd.macaroonPath}"
        ]
        ++ lib.optional (cfg.adminPasswordHashFile != null) "admin-password-hash:${cfg.adminPasswordHashFile}";
        DynamicUser = true;
        StateDirectory = "satchel";
        StateDirectoryMode = "0700";
        Restart = "on-failure";
        RestartSec = "5s";
        UMask = "0077";
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        RestrictSUIDSGID = true;
        RestrictRealtime = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        CapabilityBoundingSet = "";
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
          "~@resources"
        ];
      };
    };
  };
}
