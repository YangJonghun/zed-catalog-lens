# Catalog Lens

A Zed extension that shows the resolved version of pnpm, Yarn, and Bun `catalog:` dependencies right in package.json.

```text
"react": "catalog:react18" ^18.3.1
```

## Installation

1. Open the Extensions panel in Zed (`zed: extensions`).
2. Search for `Catalog Lens` and click **Install**.
3. Turn on inlay hints in your Zed settings (`settings.json`). Zed ships with inlay hints off.

   ```json
   { "inlay_hints": { "enabled": true } }
   ```

The first time you open a package.json, the extension downloads its language server, `catalog-lens`, from the GitHub releases. After that it reuses the downloaded server.

Catalog Lens runs on macOS (arm64, x64) and Linux (arm64, x64).

## Features

Every `catalog:` value in package.json gets:

- **Versions.** The version the catalog defines appears next to the value or above the line. Click it to jump to the definition.
- **Hover.** Hovering over a dependency shows the package, the catalog, the version, and the file and line that define it.
- **Go to definition.** Jumps from a `catalog:` value to its entry in the catalog file.
- **Diagnostics.** Warns about packages missing from a catalog, catalogs that don't exist, catalog entries the package manager would reject, and catalog files that can't be parsed.
- **Completion.** After `catalog:`, suggests only the catalogs that define that package.
- **Catalog colors.** Colors each catalog differently so you can tell them apart at a glance.

Versions update as you type in package.json, before you save. Changes to a catalog file (`pnpm-workspace.yaml`, `.yarnrc.yml`) apply when you save it.

Catalogs come from the nearest `pnpm-workspace.yaml`, `.yarnrc.yml`, or Bun root `package.json` above the file. When a folder has more than one, the `packageManager` field or the lockfile decides.

## Configuration

### Choose where versions appear

Versions can appear to the right of the value (inlay hint) or above the line (code lens).

```text
Right (inlay hint)                       Above (code lens)
                                         "react": "^18.3.1"
"react": "catalog:react18" ^18.3.1       "react": "catalog:react18"
```

By default Catalog Lens follows Zed's `code_lens` setting. With `"on"`, versions appear above the line. With `"off"` or `"menu"`, they appear on the right. Changing the setting takes effect right away. With `"menu"`, the code action menu (`cmd-.`) on a dependency line also lists the version, and choosing it jumps to the definition.

Combine Zed's settings with Catalog Lens's `display` setting to get the layout you want:

| Layout | Zed settings | Catalog Lens `display` |
| --- | --- | --- |
| Right only | `"inlay_hints": { "enabled": true }` | `"auto"` (default) or `"inlay"` |
| Above only | `"code_lens": "on"` | `"auto"` (default) or `"codeLens"` |
| Both | `"inlay_hints": { "enabled": true }`, `"code_lens": "on"` | `"both"` |

Catalog Lens can't follow the `editor: toggle code lens` command. Change Zed's `code_lens` setting instead.

### Catalog Lens settings

Put them under `lsp.catalog-lens.settings` in your Zed settings. They apply as soon as you save.

```json
{
  "inlay_hints": { "enabled": true },
  "lsp": {
    "catalog-lens": {
      "settings": {
        "display": "auto",
        "namedCatalogLabel": true
      }
    }
  }
}
```

| Key | Values | Default | Description |
| --- | --- | --- | --- |
| `display` | `"auto"`, `"inlay"`, `"codeLens"`, `"both"` | `"auto"` | Where versions appear. |
| `namedCatalogLabel` | bool | `false` | Prefixes versions from named catalogs with the catalog name, such as `react18 → ^18.3.1`. |
| `hover` | bool | `true` | Shows hover information. |
| `diagnostics` | bool | `true` | Shows diagnostics. |
| `documentColors` | bool | `true` | Colors each catalog. |
| `colorAlpha` | 0 to 1 | `0.6` | Opacity of the catalog colors. |
| `completion` | bool | `true` | Offers completions. |

`documentColors` and `colorAlpha` apply the next time you edit the file. A key with an invalid value uses its default.

### Related Zed settings

- `languages.JSON.inlay_hints.enabled` turns on inlay hints for JSON files only.
- `lsp_document_colors` picks how catalog colors are drawn.
- The theme key `editor.code_lens.foreground` sets the code lens text color.
- `lsp.catalog-lens.binary.path` runs the `catalog-lens` server at that path instead of the one on `PATH` or the downloaded one. `binary.arguments` and `binary.env` go with it.

## Troubleshooting

- **No versions appear.** Check that Zed's `inlay_hints.enabled` is on. For code lenses, set `code_lens` to `"on"`.
- **The language server fails to download.** Check that you can reach github.com, then restart Zed. If it still fails, download the `catalog-lens-<os>-<arch>.tar.gz` for your platform from the [releases page](https://github.com/YangJonghun/zed-catalog-lens/releases) and put the `catalog-lens` binary on your `PATH`, or set `lsp.catalog-lens.binary.path` to it. The extension then uses that server instead of downloading one.
- **You need more detail.** Open Zed's **LSP Logs** view and select `catalog-lens`.

If the problem persists, [open an issue](https://github.com/YangJonghun/zed-catalog-lens/issues) with your package manager, the relevant part of your catalog file, and the LSP log.

## License

[MIT](LICENSE)
