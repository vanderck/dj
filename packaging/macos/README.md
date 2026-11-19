# macOS packaging

```bash
cargo xtask package-macos --universal
```

builds dj for Apple Silicon and Intel, merges them into one binary and writes:

- `target/package/dj.app`
- `target/package/dj-<version>-macos-universal.dmg` (drag-to-Applications disk image)

Run `cargo xtask help` for all options. Useful ones:

| Option | Effect |
|---|---|
| `--model stems.onnx` | Bundle this stem-separation model in `Contents/Resources/`: the fallback when no Demucs or model is set in Settings → Stems (or `STEMS_MODEL`). |
| `--icon icon.png` | App icon: a 1024×1024 PNG (converted with `sips`/`iconutil`) or an `.icns`. Default: `packaging/macos/AppIcon.icns` (`AppIcon.png` is its 1024 px source). |
| `--sign "Developer ID Application: …"` | Sign with your Developer ID (hardened runtime). Default: `$MACOS_SIGN_IDENTITY`, else ad-hoc. |
| `--notarize` | Notarize and staple the `.dmg`. |
| `--no-dmg` | Only build the `.app`. |

Without `--universal` it builds for the Mac you are on only, which is faster.

**Intel Macs:** ONNX Runtime has no prebuilt static library for `x86_64-apple-darwin`, so the
Intel build (and the Intel half of `--universal`) is compiled without model support and separates
stems with the low-quality placeholder. Models run on Apple Silicon.

The minimum macOS version is 13.4: the statically linked ONNX Runtime used for stem separation is
built for it. The build sets `MACOSX_DEPLOYMENT_TARGET` and `LSMinimumSystemVersion` to match.

## Signing and Gatekeeper

- **Ad-hoc (default).** Runs on the Mac that built it. On other Macs Gatekeeper blocks the first
  launch: right-click the app → *Open*, or run
  `xattr -dr com.apple.quarantine /Applications/dj.app`.
- **Developer ID + notarization.** Opens anywhere without warnings. Requires an Apple Developer
  account. Store notarization credentials once:

  ```bash
  xcrun notarytool store-credentials dj --apple-id you@example.com --team-id TEAMID
  ```

  then build with:

  ```bash
  NOTARY_KEYCHAIN_PROFILE=dj cargo xtask package-macos --universal --sign "Developer ID Application: Your Name (TEAMID)" --notarize
  ```

`entitlements.plist` disables library validation: dj loads third-party VST3 plugins, which
the hardened runtime would otherwise refuse because they are signed by other developers.

## CI

`.github/workflows/macos.yml` runs the tests and builds the universal `.dmg` on every push. The
`.dmg` is attached to the workflow run as an artifact, and to a GitHub release for `v*` tags.

Optional repository secrets for signed, notarized builds:

| Secret | Value |
|---|---|
| `MACOS_CERTIFICATE` | Developer ID Application certificate + key exported as `.p12`, base64-encoded |
| `MACOS_CERTIFICATE_PASSWORD` | Password of that `.p12` |
| `MACOS_SIGN_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD` | For notarization (app-specific password) |

Optional repository variable `STEMS_MODEL_URL`: a downloadable `.onnx` model to bundle instead of
`packaging/models/stems-model.onnx`.
