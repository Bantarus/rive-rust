# Assets

The examples load a `.riv` file from a path you supply — either the **first CLI argument** or the
`RIVE_RIV` environment variable.

`.riv` test assets are intentionally **gitignored** and **not bundled** in this repo. Several of the
files used during local bring-up were third-party downloads from the Rive Community / Marketplace,
and their redistribution terms are not cleared. This `README.md` is the only file in `assets/` that
ships.

## Supplying a file

To run the examples, drop any `.riv` into this folder (or anywhere) and pass its path. You can grab a
file from:

- [rive.app/community](https://rive.app/community)
- the [awesome-rive](https://github.com/rive-app/awesome-rive) repo

## Example

```sh
cargo run -p bevy-rive --features floor --example sprite_riv -- assets/my_file.riv
```

Or via the environment variable:

```sh
RIVE_RIV=assets/my_file.riv cargo run -p bevy-rive --features floor --example sprite_riv
```
