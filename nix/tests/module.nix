{ nixpkgs, pkgs }:

let
  machine = nixpkgs.lib.nixosSystem {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      ../module.nix
      {
        system.stateVersion = "26.05";
        boot.isContainer = true;
        services.satchel = {
          enable = true;
          package = pkgs.writeShellScriptBin "satchel" "exit 0";
          publicUrl = "https://wallet.example.org";
          operatorUrl = "https://wallet-admin.example.org:9443";
          handoffOrigins = [ "https://app.example.org" ];
          metricsAddress = "127.0.0.1:9095";
          adminPasswordHashFile = "/run/secrets/wallet-admin.hash";
          lnd = {
            restHost = "127.0.0.1:8080";
            tlsCertPath = "/run/lnd/tls.cert";
            macaroonPath = "/run/lnd/wallet.macaroon";
            expectedNetwork = "signet";
          };
          faucet = {
            enabled = true;
            amount_sat = 5000;
          };
          pow.base_bits = 16;
        };
      }
    ];
  };
  config = machine.config;
  service = config.systemd.services.satchel.serviceConfig;
  configFile = nixpkgs.lib.last (nixpkgs.lib.splitString " " service.ExecStart);
in
assert nixpkgs.lib.all (item: item.assertion) config.assertions;
assert service.DynamicUser;
assert service.StateDirectory == "satchel";
assert service.ProtectSystem == "strict";
assert
  service.LoadCredential == [
    "lnd-tls:/run/lnd/tls.cert"
    "lnd-macaroon:/run/lnd/wallet.macaroon"
    "admin-password-hash:/run/secrets/wallet-admin.hash"
  ];
assert config.networking.firewall.allowedTCPPorts == [ ];
pkgs.runCommand "satchel-module-check"
  {
    nativeBuildInputs = [ pkgs.python3 ];
  }
  ''
    python - ${configFile} <<'PY'
    import sys
    import tomllib

    with open(sys.argv[1], "rb") as source:
        config = tomllib.load(source)
    server = config["server"]
    assert server["public_url"] == "https://wallet.example.org"
    assert server["operator_url"] == "https://wallet-admin.example.org:9443"
    assert server["bind_address"] == "127.0.0.1:8095"
    assert server["database_path"] == "/var/lib/satchel/wallet.db"
    assert server["metrics_address"] == "127.0.0.1:9095"
    assert server["admin_password_hash_file"] == "admin-password-hash"
    assert server["handoff_origins"] == ["https://app.example.org"]
    assert "client_ip_header" not in server
    lnd = config["lnd"]
    assert lnd["rest_host"] == "127.0.0.1:8080"
    assert lnd["tls_cert_path"] == "lnd-tls"
    assert lnd["macaroon_path"] == "lnd-macaroon"
    assert lnd["expected_network"] == "signet"
    assert config["faucet"] == {"enabled": True, "amount_sat": 5000}
    assert config["limits"] == {}
    assert config["pow"] == {"base_bits": 16}
    assert config["rate_limits"] == {}
    assert "/run/" not in str(config)
    PY
    touch "$out"
  ''
