# Donated fixtures

A RAW goes here only when its donor gave explicit consent to a public release and no CC0 host already serves the file. The consent, license, sha256 and byte size are recorded in `fixtures/manifest.toml` with `committed = "lfs"`; the in-app donation flow writes that placement when a moderator approves an uploaded RAW.

`.gitattributes` routes `fixtures/donated/**` through Git LFS. Run `git lfs pull` after cloning, then `tools/fetch-fixtures.sh --check` to confirm each donated file matches its manifest digest; the script verifies LFS entries but never downloads them.

An identical sample with a stable public host stays fetch-only (`committed = false`), and a test vector under 1 MB that needs to be a compile-time input stays in plain git.
