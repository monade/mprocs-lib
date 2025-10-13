
## Release process

1. check all is ok, build with `--release` and run `cargo check`
2. update changelog and bump version in src/Cargo.toml
3. add, commit, tag with version and push
3. run `cargo publish -p monade-mprocs`

