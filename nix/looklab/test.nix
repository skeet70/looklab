# SPDX-License-Identifier: AGPL-3.0-or-later
{ self, nixpkgs }:

let
  pkgs = import nixpkgs { system = "x86_64-linux"; };
in
pkgs.testers.runNixOSTest {
  name = "looklab-deploy";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ self.nixosModules.looklab ];

      services.looklab = {
        enable = true;
        hostname = "looklab.test";
        caddy.enable = false;
        worker.memoryMax = "3G";
        worker.memoryHigh = "2G";
      };

      environment.systemPackages = [
        pkgs.curl
        pkgs.jq
        pkgs.sqlite
      ];
      virtualisation.memorySize = 4096;
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("looklab-seed-data.service")
    machine.wait_for_unit("looklab-server.service")
    machine.wait_for_unit("looklab-worker.service")
    machine.wait_for_unit("looklab-publisher.service")
    machine.succeed(
        "systemctl show looklab-worker.service -p Environment "
        "| grep -q 'LOOKLAB_TRIM_AFTER_JOB=1'"
    )
    machine.wait_for_file("/var/lib/looklab/looklab.sqlite3")
    machine.wait_for_file("/var/lib/looklab/data/bodies")

    machine.wait_until_succeeds("curl -fsS http://127.0.0.1:8080/healthz", timeout=120)
    machine.wait_until_succeeds("curl -fsS http://127.0.0.1:8080/readyz", timeout=300)
    machine.succeed("curl -fsS http://127.0.0.1:8080/ | grep -qi '<html'")
    machine.succeed("curl -fsS http://127.0.0.1:8080/data/index.json | jq -e '.format == \"looklab-site/1\"'")
    machine.succeed("curl -fsS http://127.0.0.1:8080/data/site-root.json > /dev/null")

    machine.wait_for_file("/var/lib/looklab/data/scenes/index.json")
    machine.wait_for_file("/var/lib/looklab/data/manuals/fujifilm-x-t5.yaml")
    machine.succeed(
        "curl -fsS http://127.0.0.1:8080/data/index.json "
        "| jq -e 'any(.manuals[]; .id == \"fujifilm/x-t5\")'"
    )
    machine.succeed(
        "curl -fsS http://127.0.0.1:8080/data/manuals/fujifilm/x-t5.json "
        "| jq -e '.id == \"fujifilm/x-t5\" and .documentation_derived "
        "and ([.sources[].url | startswith(\"https://\")] | all) "
        "and ([.sources[].firmware_scope | length > 0] | all) "
        "and ([.capabilities[] | select(.key == \"native_simulations\") "
        "| select(.availability == \"available\") "
        "| select(.domain.values | index(\"PROVIA/STANDARD\")) "
        "| select((.citations | length) > 0)] | length == 1)'"
    )
    machine.succeed(
        "curl -fsS http://127.0.0.1:8080/api/manuals/fujifilm/x-t5 "
        "| jq -e '.id == \"fujifilm/x-t5\" and .documentation_derived "
        "and .human_confirmed == false "
        "and ([.sources[].url | startswith(\"https://\")] | all) "
        "and ([.sources[].firmware_scope | length > 0] | all) "
        "and ([.capabilities[] | select(.key == \"native_simulations\") "
        "| select(.availability == \"available\") "
        "| select(.domain.values | index(\"PROVIA/STANDARD\")) "
        "| select((.citations | length) > 0)] | length == 1)'"
    )
    machine.succeed("curl -fsS http://127.0.0.1:8080/data/index.json | jq -e '.scenes | length == 9'")
    machine.succeed(
        "curl -fsS http://127.0.0.1:8080/data/index.json "
        "| jq -e '[.scenes[] | select(.kind == \"photo\")] | length == 6'"
    )
    machine.succeed(
        "curl -fsS http://127.0.0.1:8080/data/index.json "
        "| jq -e '[.scenes[] | select(.kind == \"test_chart\")] | length == 3'"
    )
    machine.succeed(
        "curl -fsS http://127.0.0.1:8080/data/index.json "
        "| jq -e '[.scenes[] | select(.kind == \"photo\") | select(.credit.license == \"CC0-1.0\") "
        "| select(.credit.source_url | test(\"^https://\")) "
        "| select(.credit.source_sha256 | test(\"^[0-9a-f]{64}$\")) "
        "| select(.sha256 | test(\"^[0-9a-f]{64}$\"))] | length == 6'"
    )
    machine.succeed(
        "set -eu; "
        "index=$(curl -fsS http://127.0.0.1:8080/data/index.json); "
        "path=$(printf '%s' \"$index\" | jq -r '[.scenes[] | select(.kind == \"photo\")][0].path'); "
        "want=$(printf '%s' \"$index\" | jq -r '[.scenes[] | select(.kind == \"photo\")][0].sha256'); "
        "got=$(curl -fsS \"http://127.0.0.1:8080/$path\" | sha256sum | cut -d' ' -f1); "
        "test \"$got\" = \"$want\""
    )

    machine.succeed("stat -c %a /var/lib/looklab | grep -qx 700")
    machine.succeed("stat -c %a /var/lib/looklab/server-secret | grep -qx 600")
    owner = machine.succeed("stat -c '%U:%G' /var/lib/looklab/server-secret").strip()
    assert owner == "looklab:looklab", f"server-secret is owned by {owner}"
    machine.succeed("stat -c %a /var/lib/looklab/data | grep -qx 700")
    manual = "/var/lib/looklab/data/manuals/fujifilm-x-m1.yaml"
    manual_hash = machine.succeed(f"sha256sum {manual}").split()[0]
    body = "/var/lib/looklab/data/bodies/fujifilm-x-t4.body.yaml"
    body_hash = machine.succeed(f"sha256sum {body}").split()[0]
    host_record = "/var/lib/looklab/data/manuals/operator-note.txt"
    machine.succeed(f"install -o looklab -g looklab -m 0400 /dev/null {host_record}")
    machine.succeed(f"rm {manual}")
    packaged_transform = "/var/lib/looklab/data/transforms/fujifilm-x-m1.yaml"
    packaged_transform_hash = machine.succeed(f"sha256sum {packaged_transform}").split()[0]
    machine.succeed(f"rm {packaged_transform}")
    host_transform = "/var/lib/looklab/data/transforms/fujifilm-x-t4.yaml"
    machine.succeed(f"echo '# operator edit' >> {host_transform}")
    host_transform_hash = machine.succeed(f"sha256sum {host_transform}").split()[0]
    for _ in range(2):
        machine.succeed("systemctl restart looklab-seed-data.service")
        assert manual_hash == machine.succeed(f"sha256sum {manual}").split()[0]
        assert "400" == machine.succeed(f"stat -c %a {host_record}").strip()
        assert body_hash == machine.succeed(f"sha256sum {body}").split()[0]
        assert packaged_transform_hash == machine.succeed(f"sha256sum {packaged_transform}").split()[0]
        assert host_transform_hash == machine.succeed(f"sha256sum {host_transform}").split()[0]

    # Restoring seed data can request a publication; let it finish so the
    # restart below is the only change under test.
    pending = machine.succeed(
        "sqlite3 -cmd '.timeout 5000' 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' "
        "\"SELECT generation FROM publication WHERE id = 1\""
    ).strip()
    machine.wait_until_succeeds(
        "sqlite3 -cmd '.timeout 5000' 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' "
        f"\"SELECT published >= {pending} FROM publication WHERE id = 1\" | grep -qx 1",
        timeout=120,
    )
    machine.succeed("install -o looklab -g looklab -m 0600 /dev/null /var/lib/looklab/state-marker")
    secret = machine.succeed("sha256sum /var/lib/looklab/server-secret").split()[0]
    index = machine.succeed("sha256sum /var/lib/looklab/site/data/index.json").split()[0]
    migrations = machine.succeed(
        "sqlite3 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' 'select count(*) from _sqlx_migrations'"
    ).strip()

    machine.succeed(
        "systemctl restart looklab-worker.service looklab-publisher.service looklab-server.service"
    )
    machine.wait_for_unit("looklab-worker.service")
    machine.wait_for_unit("looklab-publisher.service")
    machine.wait_for_unit("looklab-server.service")
    machine.wait_until_succeeds("curl -fsS http://127.0.0.1:8080/readyz", timeout=300)

    assert secret == machine.succeed("sha256sum /var/lib/looklab/server-secret").split()[0]
    assert index == machine.succeed("sha256sum /var/lib/looklab/site/data/index.json").split()[0]
    assert migrations == machine.succeed(
        "sqlite3 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' 'select count(*) from _sqlx_migrations'"
    ).strip()
    machine.succeed("test -e /var/lib/looklab/state-marker")
    machine.succeed("curl -fsS http://127.0.0.1:8080/ | grep -qi '<html'")
    machine.succeed("curl -fsS http://127.0.0.1:8080/data/index.json | jq -e '.format == \"looklab-site/1\"'")

    # Publication must proceed while the fit lane is occupied. A running
    # consistency fit holds an expired lease that only its lane may reclaim,
    # and a queued fit job sorts before every publication request.
    machine.succeed("systemctl stop looklab-worker.service")
    machine.succeed(
        "sqlite3 -cmd '.timeout 5000' /var/lib/looklab/looklab.sqlite3 "
        "\"INSERT INTO jobs (id, kind, payload, status, attempts, run_after, locked_until, created_at) "
        "VALUES ('expired-fit', 'consistency_score', '{}', 'running', 1, 0, 1, 0)\""
    )
    machine.succeed(
        "sqlite3 -cmd '.timeout 5000' /var/lib/looklab/looklab.sqlite3 "
        "\"INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at) "
        "VALUES ('a-queued-fit', 'consistency_score', '{}', 'queued', 0, 0, 0)\""
    )

    machine.succeed(
        "curl -fsS http://127.0.0.1:8080/data/index.json "
        "| jq -e '([.manuals[].id] | index(\"fujifilm/x-m1\")) != null'"
    )
    machine.succeed("rm /var/lib/looklab/data/manuals/fujifilm-x-m1.yaml")
    machine.succeed(
        "sqlite3 -cmd '.timeout 5000' /var/lib/looklab/looklab.sqlite3 "
        "\"UPDATE publication SET generation = generation + 1 WHERE id = 1\""
    )
    generation = machine.succeed(
        "sqlite3 -cmd '.timeout 5000' 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' "
        "\"SELECT generation FROM publication WHERE id = 1\""
    ).strip()
    machine.wait_until_succeeds(
        "sqlite3 -cmd '.timeout 5000' 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' "
        f"\"SELECT published >= {generation} FROM publication WHERE id = 1\" | grep -qx 1",
        timeout=120,
    )
    published = machine.succeed("sha256sum /var/lib/looklab/site/data/index.json").split()[0]
    assert published != index, "the publication did not rewrite the index"
    machine.succeed(
        "curl -fsS http://127.0.0.1:8080/data/index.json "
        "| jq -e '([.manuals[].id] | index(\"fujifilm/x-m1\")) == null'"
    )
    fit_state = machine.succeed(
        "sqlite3 -cmd '.timeout 5000' 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' "
        "\"SELECT status || ':' || attempts FROM jobs WHERE id = 'a-queued-fit'\""
    ).strip()
    assert fit_state == "queued:0", f"the publish lane claimed a fit job: {fit_state}"
    expired_state = machine.succeed(
        "sqlite3 -cmd '.timeout 5000' 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' "
        "\"SELECT status || ':' || attempts FROM jobs WHERE id = 'expired-fit'\""
    ).strip()
    assert expired_state == "running:1", f"the fit lease changed: {expired_state}"

    # Restore the manual and publish again so the catalog is complete.
    machine.succeed("systemctl restart looklab-seed-data.service")
    machine.succeed(
        "sqlite3 -cmd '.timeout 5000' /var/lib/looklab/looklab.sqlite3 "
        "\"UPDATE publication SET generation = generation + 1 WHERE id = 1\""
    )
    generation = machine.succeed(
        "sqlite3 -cmd '.timeout 5000' 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' "
        "\"SELECT generation FROM publication WHERE id = 1\""
    ).strip()
    machine.wait_until_succeeds(
        "sqlite3 -cmd '.timeout 5000' 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' "
        f"\"SELECT published >= {generation} FROM publication WHERE id = 1\" | grep -qx 1",
        timeout=120,
    )
    machine.succeed(
        "curl -fsS http://127.0.0.1:8080/data/index.json "
        "| jq -e '([.manuals[].id] | index(\"fujifilm/x-m1\")) != null'"
    )
  '';
}
