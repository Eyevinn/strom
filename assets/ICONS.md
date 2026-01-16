# Strom Icons

Icon assets for the Strom application across all platforms.

## Icon Files

| File | Purpose | Sizes |
|------|---------|-------|
| `strom-icon.svg` | Main app icon (desktop, large displays) | 512x512 base |
| `favicon.svg` | Browser tab favicon | 32x32 optimized |
| `apple-touch-icon.svg` | iOS PWA home screen | 180x180 |
| `icon-maskable.svg` | Android PWA (adaptive icon) | 512x512 with safe zone |

## Generating PNG/ICO Files

### Using ImageMagick

```bash
# Desktop icons (PNG)
convert -background none strom-icon.svg -resize 16x16 icon-16.png
convert -background none strom-icon.svg -resize 32x32 icon-32.png
convert -background none strom-icon.svg -resize 64x64 icon-64.png
convert -background none strom-icon.svg -resize 128x128 icon-128.png
convert -background none strom-icon.svg -resize 256x256 icon-256.png
convert -background none strom-icon.svg -resize 512x512 icon-512.png

# Windows ICO (multi-size)
convert icon-16.png icon-32.png icon-64.png icon-256.png strom.ico

# macOS ICNS
mkdir strom.iconset
cp icon-16.png strom.iconset/icon_16x16.png
cp icon-32.png strom.iconset/icon_16x16@2x.png
cp icon-32.png strom.iconset/icon_32x32.png
cp icon-64.png strom.iconset/icon_32x32@2x.png
cp icon-128.png strom.iconset/icon_128x128.png
cp icon-256.png strom.iconset/icon_128x128@2x.png
cp icon-256.png strom.iconset/icon_256x256.png
cp icon-512.png strom.iconset/icon_256x256@2x.png
cp icon-512.png strom.iconset/icon_512x512.png
iconutil -c icns strom.iconset

# iOS PWA
convert -background none apple-touch-icon.svg -resize 180x180 apple-touch-icon.png

# Android PWA
convert -background none icon-maskable.svg -resize 192x192 icon-192.png
convert -background none icon-maskable.svg -resize 512x512 icon-512-maskable.png

# Favicon
convert -background none favicon.svg -resize 32x32 favicon.png
convert favicon.png favicon.ico
```

### Using rsvg-convert (Linux)

```bash
rsvg-convert -w 512 -h 512 strom-icon.svg -o icon-512.png
rsvg-convert -w 32 -h 32 favicon.svg -o favicon.png
```

## HTML Usage

```html
<!-- Favicon -->
<link rel="icon" type="image/svg+xml" href="/assets/favicon.svg">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/favicon.png">
<link rel="icon" type="image/x-icon" href="/assets/favicon.ico">

<!-- iOS PWA -->
<link rel="apple-touch-icon" href="/assets/apple-touch-icon.png">

<!-- Theme color -->
<meta name="theme-color" content="#0d1b2a">
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
    },
    {
      "src": "/assets/icon-512-maskable.png",
      "sizes": "512x512",
      "type": "image/png",
      "purpose": "maskable"
    }
  ],
  "theme_color": "#0d1b2a",
  "background_color": "#0d1b2a"
}
```

## Design Notes

The icon represents "ström" (Swedish) which means both:
1. **Stream** - flow of water, data streaming
2. **Electric current** - power, energy

The lightning bolt shape captures both meanings - it flows like a stream while representing electrical current. The cyan-to-purple gradient suggests energy and modern technology.
