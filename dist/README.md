# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/hearth-<version>-linux-<arch>.tar.gz` containing:

- `hearth` — the binary (headed by default; `hearth headless` runs the engine alone)
- `hearth.desktop` — XDG desktop entry
- `hearth.png` — 1024×1024 Hearth app icon
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/hearth-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Hearth.app` (Info.plist + icns), ad-hoc
signs it (set `CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a
dmg. The auto-update tarball retains an internal `Hearth.app` path so older
installed builds can update into Hearth. CI runs this on tags
(`.github/workflows/release.yml`). The manual steps it automates, for reference
(run on a macOS host — gpui needs Metal; no cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p hearth --target aarch64-apple-darwin
   cargo build --release -p hearth --target x86_64-apple-darwin
   lipo -create -output hearth \
     target/aarch64-apple-darwin/release/hearth \
     target/x86_64-apple-darwin/release/hearth
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Hearth.app/Contents/{MacOS,Resources}
   cp hearth Hearth.app/Contents/MacOS/hearth
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Hearth.app/Contents/Info.plist
   ```
3. Icon: generate `hearth.icns` from `dist/macos/icon-1024.png` (the macOS-shaped
   variant of the artwork — squircle mask, margins, and shadow pre-baked, since
   `sips` can't apply an alpha mask) and place it at
   `Hearth.app/Contents/Resources/hearth.icns`:
   ```sh
   mkdir hearth.iconset && sips -z 256 256 dist/macos/icon-1024.png --out hearth.iconset/icon_256x256.png
   iconutil -c icns hearth.iconset -o Hearth.app/Contents/Resources/hearth.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Hearth.app
   xcrun notarytool submit Hearth.zip --keychain-profile … --wait
   xcrun stapler staple Hearth.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Hearth -srcfolder Hearth.app -ov -format UDZO Hearth.dmg`).
