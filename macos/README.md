# Mercator

A Mac app that starts the Mercator binary and shows the dashboard at `http://127.0.0.1:3000/`.

## Stage the binary and dashboard

From the repository root, build the release binary, then copy it and `dist/` into the app:

```sh
cargo build --release
macos/scripts/stage-assets.sh
```

Run that script again after either file changes. The script is the copy step. The copies under `macos/assets/` are local build output.

## Run

```sh
cd macos
native dev
```

The app listens on `127.0.0.1:3000`. If that port is already taken, the window shows the spawn failure. The other process is left running, and the app does not replace an existing database.
