# Strom Icons

Icon assets for the Strom application across all platforms.

## Design

The icon features:
- **S** for Strom (Swedish: ström = stream / electric current)
- **Circular arrows** representing streaming/flow
- Metallic silver on dark blue gradient

## Icon Files

| File | Size | Usage |
|------|------|-------|
| `strom-icon-1024.png` | 1024x1024 | Source/master icon |
| `icon-512.png` | 512x512 | Android, macOS |
| `icon-192.png` | 192x192 | Android PWA |
| `apple-touch-icon-180.png` | 180x180 | iOS PWA, Safari |
| `icon-128.png` | 128x128 | Desktop apps |
| `icon-64.png` | 64x64 | Desktop apps |
| `favicon-32.png` | 32x32 | Browser tab |
| `favicon-16.png` | 16x16 | Browser tab |
| `favicon.ico` | 16/32/48 | Browser favicon, legacy |
| `strom.ico` | 16-256 | Windows desktop app |

## HTML Usage

```html
<!-- Favicon -->
<link rel="icon" type="image/x-icon" href="/assets/favicon.ico">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/favicon-32.png">
<link rel="icon" type="image/png" sizes="16x16" href="/assets/favicon-16.png">

<!-- iOS/Safari -->
<link rel="apple-touch-icon" sizes="180x180" href="/assets/apple-touch-icon-180.png">

<!-- Theme color -->
<meta name="theme-color" content="#3b82f6">
```

## Web App Manifest (manifest.json)

```json
{
  "name": "Strom",
  "short_name": "Strom",
  "icons": [
    {
      "src": "/assets/icon-192.png",
      "sizes": "192x192",
      "type": "image/png"
    },
    {
      "src": "/assets/icon-512.png",
      "sizes": "512x512",
      "type": "image/png"
    }
  ],
  "theme_color": "#3b82f6",
  "background_color": "#1e3a8a"
}
```

## Regenerating Icons

To regenerate all icons from the master `strom-icon-1024.png`:

```bash
cargo run --bin gen-icons
```

This generates:
- All PNG sizes (512, 192, 180, 128, 64, 32, 16)
- ICO files (favicon.ico, strom.ico)
- Frontend icon (frontend/src/icon.png)

**Workflow for updating icons:**
1. Replace `assets/strom-icon-1024.png` with new 1024x1024 source
2. Run `cargo run --bin gen-icons`
3. Commit all changed files

Alternative icon designs are stored in `assets/other/` for reference.
